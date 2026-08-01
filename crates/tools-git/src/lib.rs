//! Typed repository-local Git tools over an injected workspace root.
//!
//! Repository discovery and linked worktrees are deliberately unsupported. A
//! suite binds one direct main worktree whose .git directory is inside the
//! injected root. The local family has no remote operation.

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet, HashSet},
    error::Error,
    ffi::{OsStr, OsString},
    fmt, fs,
    io::{Read, Seek, Write},
    os::fd::{AsRawFd, OwnedFd},
    os::unix::ffi::{OsStrExt, OsStringExt},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

use git2::{
    Buf, CheckoutNotificationType, Config, Delta, DiffFindOptions, DiffFormat, DiffOptions,
    ErrorCode, Index, IndexEntry, IndexTime, Indexer, Mempack, Odb, PackBuilder, Patch, Repository,
    RepositoryOpenFlags, RepositoryState, Signature, build::CheckoutBuilder,
};
use rustix::fs::{
    AtFlags, CWD, Dir, Mode, OFlags, RenameFlags, mkdirat, openat, readlinkat_raw, renameat_with,
    unlinkat,
};
use rustix::io::dup;
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
    LocalWorkspaceFileSystem, WorkspaceEntryKind, WorkspaceFileSystem, WorkspacePathRejection,
    WorkspaceResolveError, WorkspaceRoot, WorkspaceRootError,
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
const MAX_SHALLOW_ENTRIES: usize = 1024;
const MAX_SHALLOW_BYTES: usize = MAX_SHALLOW_ENTRIES * 41;
const MAX_INDEX_BYTES: usize = 64 * 1024 * 1024;
const MAX_INDEX_ENTRIES: usize = MAX_WORKTREE_INSPECTIONS;
const MAX_OBJECT_BYTES: usize = 1024 * 1024;
const MAX_PACK_FILE_BYTES: usize = MAX_OBJECT_DATABASE_BYTES;
const MAX_OBJECT_DATABASE_BYTES: usize = 128 * MAX_OBJECT_BYTES;
const MAX_TREE_BLOB_BYTES: usize = 64 * MAX_OBJECT_BYTES;
const MAX_REFLOG_BYTES: usize = 64 * MAX_OBJECT_BYTES;
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
const INDEX_ASSUME_VALID: u16 = 1 << 15;
const INDEX_SKIP_WORKTREE: u16 = 1 << 14;
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
        && path
            .components()
            .next()
            .is_some_and(|component| !is_repository_administration_component(component))
    {
        Ok(path.to_owned())
    } else {
        Err(InvalidGitArguments)
    }
}

fn is_repository_administration_component(component: Component<'_>) -> bool {
    matches!(
        component,
        Component::Normal(name) if name.as_bytes().eq_ignore_ascii_case(b".git")
    )
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
    root: fs::File,
    git_directory: fs::File,
    _config: fs::File,
    repository: Mutex<Repository>,
}

struct PinnedObjectDatabase {
    directory: tempfile::TempDir,
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
            root,
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

impl PinnedObjectDatabase {
    fn capture(authority: &PinnedRepository) -> Result<Self, LocalGitFailure> {
        let objects = openat(
            &authority.git_directory,
            "objects",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Repository)?;
        let directory = tempfile::tempdir().map_err(|_| LocalGitFailure::Operation)?;
        fs::create_dir(directory.path().join("pack")).map_err(|_| LocalGitFailure::Operation)?;
        let mut inspected = 0_usize;
        let mut captured_bytes = 0_u64;
        for entry in fs::read_dir(descriptor_path_from_fd(&objects))
            .map_err(|_| LocalGitFailure::Repository)?
        {
            let entry = entry.map_err(|_| LocalGitFailure::Repository)?;
            inspected = inspected.saturating_add(1);
            if inspected > 100_000 {
                return Err(LocalGitFailure::Repository);
            }
            let name = entry.file_name();
            let bytes = name.as_bytes();
            if name == OsStr::new("info") {
                continue;
            }
            if name == OsStr::new("pack") {
                let pack = openat(
                    &objects,
                    &name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|_| LocalGitFailure::Repository)?;
                pin_object_directory(
                    &pack,
                    &directory.path().join("pack"),
                    &mut inspected,
                    &mut captured_bytes,
                    false,
                )?;
                continue;
            }
            if bytes.len() != 2 || !bytes.iter().all(u8::is_ascii_hexdigit) {
                return Err(LocalGitFailure::Repository);
            }
            let loose = openat(
                &objects,
                &name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| LocalGitFailure::Repository)?;
            let destination = directory.path().join(&name);
            fs::create_dir(&destination).map_err(|_| LocalGitFailure::Operation)?;
            pin_object_directory(
                &loose,
                &destination,
                &mut inspected,
                &mut captured_bytes,
                true,
            )?;
        }
        Ok(Self { directory })
    }

    fn add_to(&self, object_database: &Odb<'_>) -> Result<(), LocalGitFailure> {
        let path = self
            .directory
            .path()
            .to_str()
            .ok_or(LocalGitFailure::Operation)?;
        object_database
            .add_disk_alternate(path)
            .map_err(|_| LocalGitFailure::Operation)
    }
}

fn descriptor_path_from_fd(file: &OwnedFd) -> PathBuf {
    PathBuf::from(format!("/dev/fd/{}", file.as_raw_fd()))
}

fn pin_object_directory(
    source: &OwnedFd,
    destination: &Path,
    inspected: &mut usize,
    captured_bytes: &mut u64,
    loose: bool,
) -> Result<(), LocalGitFailure> {
    for entry in
        fs::read_dir(descriptor_path_from_fd(source)).map_err(|_| LocalGitFailure::Repository)?
    {
        let entry = entry.map_err(|_| LocalGitFailure::Repository)?;
        *inspected = inspected.saturating_add(1);
        if *inspected > 100_000 {
            return Err(LocalGitFailure::Repository);
        }
        let name = entry.file_name();
        if loose
            && (name.as_bytes().len() != 38 || !name.as_bytes().iter().all(u8::is_ascii_hexdigit))
        {
            return Err(LocalGitFailure::Repository);
        }
        let descriptor = openat(
            source,
            &name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Repository)?;
        let mut file = fs::File::from(descriptor);
        let metadata = file.metadata().map_err(|_| LocalGitFailure::Repository)?;
        let per_file_limit = if loose {
            MAX_OBJECT_BYTES.saturating_mul(2)
        } else {
            MAX_PACK_FILE_BYTES
        } as u64;
        if !metadata.is_file()
            || metadata.len() > per_file_limit
            || captured_bytes.saturating_add(metadata.len()) > MAX_OBJECT_DATABASE_BYTES as u64
        {
            return Err(LocalGitFailure::Repository);
        }
        let mut snapshot = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination.join(&name))
            .map_err(|_| LocalGitFailure::Operation)?;
        let copied = std::io::copy(
            &mut Read::by_ref(&mut file).take(metadata.len().saturating_add(1)),
            &mut snapshot,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        if copied != metadata.len() {
            return Err(LocalGitFailure::Repository);
        }
        *captured_bytes = captured_bytes.saturating_add(copied);
    }
    Ok(())
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
    let git_directory = openat(
        CWD,
        &dot_git,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if dot_git.join("commondir").exists() || dot_git.join("objects/info/alternates").exists() {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    reject_administrative_symlinks(&git_directory)?;
    reject_escaping_config(&dot_git.join("config"))?;
    Ok(RepositoryIdentity {
        root: root_identity,
        git_directory: git_directory_identity,
    })
}

fn reject_administrative_symlinks(
    git_directory: &OwnedFd,
) -> Result<(), LocalGitToolsConstructionError> {
    let root = dup(git_directory).map_err(|_| LocalGitToolsConstructionError::Repository)?;
    let mut pending = vec![(root, PathBuf::new())];
    let mut inspected = 0_usize;
    while let Some((current, relative_directory)) = pending.pop() {
        let mut entries =
            Dir::read_from(&current).map_err(|_| LocalGitToolsConstructionError::Repository)?;
        while let Some(entry) = entries.read() {
            let entry = entry.map_err(|_| LocalGitToolsConstructionError::Repository)?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes());
            if name == OsStr::new(".") || name == OsStr::new("..") {
                continue;
            }
            inspected = inspected.saturating_add(1);
            if inspected > 100_000 {
                return Err(LocalGitToolsConstructionError::Repository);
            }
            let mut relative = relative_directory.clone();
            relative.push(name);
            match openat(
                &current,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(directory) => {
                    pending.push((directory, relative));
                    continue;
                }
                Err(error) if error == rustix::io::Errno::NOTDIR => {}
                Err(_) => return Err(LocalGitToolsConstructionError::Repository),
            }
            let descriptor = openat(
                &current,
                name,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
            let mut file = fs::File::from(descriptor);
            let metadata = file
                .metadata()
                .map_err(|_| LocalGitToolsConstructionError::Repository)?;
            if !metadata.is_file() {
                return Err(LocalGitToolsConstructionError::Repository);
            }
            let limit = if relative == Path::new("HEAD") || relative.starts_with("refs") {
                Some(MAX_REVISION_BYTES)
            } else if relative == Path::new("packed-refs") {
                Some(MAX_PACKED_REFS_BYTES)
            } else if relative == Path::new("shallow") {
                Some(MAX_SHALLOW_BYTES)
            } else {
                None
            };
            if limit.is_some_and(|limit| metadata.len() > limit as u64) {
                return Err(LocalGitToolsConstructionError::Repository);
            }
            if relative == Path::new("shallow") {
                validate_shallow_file(&mut file)?;
            }
        }
    }
    Ok(())
}

fn validate_shallow_file(file: &mut fs::File) -> Result<(), LocalGitToolsConstructionError> {
    let mut bytes = Vec::with_capacity(MAX_SHALLOW_BYTES);
    Read::by_ref(file)
        .take((MAX_SHALLOW_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if bytes.len() > MAX_SHALLOW_BYTES {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    let mut entries = 0_usize;
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        entries = entries.saturating_add(1);
        if entries > MAX_SHALLOW_ENTRIES
            || line.len() != 40
            || !line.iter().all(u8::is_ascii_hexdigit)
        {
            return Err(LocalGitToolsConstructionError::Repository);
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
        let mut normalized = line.trim().to_ascii_lowercase();
        if normalized.starts_with('[') {
            let closing = normalized
                .find(']')
                .ok_or(LocalGitToolsConstructionError::Repository)?;
            let header = &normalized[..=closing];
            section = if header.starts_with("[core]") {
                "core"
            } else if header.starts_with("[extensions]") {
                "extensions"
            } else if header.starts_with("[filter ") || header.starts_with("[include") {
                return Err(LocalGitToolsConstructionError::Repository);
            } else {
                ""
            };
            let trailing = normalized[closing + 1..].trim();
            if trailing.is_empty() || trailing.starts_with('#') || trailing.starts_with(';') {
                continue;
            }
            normalized = trailing.to_owned();
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
        let pinned_objects = PinnedObjectDatabase::capture(&self.repository_authority)?;
        let persistent_object_database = Odb::new().map_err(|_| LocalGitFailure::Operation)?;
        pinned_objects.add_to(&persistent_object_database)?;
        let object_database = Odb::new().map_err(|_| LocalGitFailure::Operation)?;
        pinned_objects.add_to(&object_database)?;
        let mempack = object_database
            .add_new_mempack_backend(1000)
            .map_err(|_| LocalGitFailure::Operation)?;
        repository
            .set_odb(&object_database)
            .map_err(|_| LocalGitFailure::Operation)?;
        let result = match operation {
            LocalOperation::Status => {
                let _index_snapshot = self.bind_index_snapshot(&repository)?;
                let untracked = self.discover_untracked_paths(&repository)?;
                LocalGitResult::Status(status(
                    &repository,
                    &self.repository_authority,
                    &self.filesystem,
                    &self.root,
                    untracked,
                )?)
            }
            LocalOperation::Diff(arguments) => {
                let index_snapshot = if matches!(arguments, GitDiffArguments::Worktree) {
                    Some(self.bind_index_snapshot(&repository)?)
                } else {
                    None
                };
                let untracked = if index_snapshot.is_some() {
                    self.discover_untracked_paths(&repository)?
                } else {
                    Vec::new()
                };
                LocalGitResult::Diff(diff(
                    &repository,
                    &self.repository_authority,
                    arguments,
                    &self.filesystem,
                    &self.root,
                    untracked,
                )?)
            }
            LocalOperation::Log(arguments) => {
                LocalGitResult::Log(log(&repository, &self.repository_authority, arguments)?)
            }
            LocalOperation::Stage(arguments) => {
                let result = LocalGitResult::Stage(self.stage_with_pinned_objects(
                    &repository,
                    &persistent_object_database,
                    &object_database,
                    &mempack,
                    arguments,
                    || {},
                )?);
                return encode_result(&result);
            }
            LocalOperation::Commit(arguments) => {
                let result = LocalGitResult::Commit(commit(
                    &mut repository,
                    &self.identity,
                    arguments,
                    &self.repository_authority,
                    &persistent_object_database,
                    &object_database,
                    || self.validate_current_repository_identity(),
                )?);
                return encode_result(&result);
            }
            LocalOperation::BranchCreate(arguments) => {
                let result = LocalGitResult::BranchCreate(branch_create(
                    &repository,
                    &self.repository_authority,
                    arguments,
                    || self.validate_current_repository_identity(),
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

    #[cfg(test)]
    fn stage(
        &self,
        repository: &Repository,
        arguments: GitStageArguments,
    ) -> Result<StageResult, LocalGitFailure> {
        self.stage_with_pre_publish_hook(repository, arguments, || {})
    }

    #[cfg(test)]
    fn stage_with_pre_publish_hook<BeforePublish>(
        &self,
        repository: &Repository,
        arguments: GitStageArguments,
        before_publish: BeforePublish,
    ) -> Result<StageResult, LocalGitFailure>
    where
        BeforePublish: FnOnce(),
    {
        let pinned_objects = PinnedObjectDatabase::capture(&self.repository_authority)?;
        let persistent_object_database = Odb::new().map_err(|_| LocalGitFailure::Operation)?;
        pinned_objects.add_to(&persistent_object_database)?;
        let object_database = repository.odb().map_err(|_| LocalGitFailure::Operation)?;
        let mempack = object_database
            .add_new_mempack_backend(1000)
            .map_err(|_| LocalGitFailure::Operation)?;
        self.stage_with_pinned_objects(
            repository,
            &persistent_object_database,
            &object_database,
            &mempack,
            arguments,
            before_publish,
        )
    }

    fn stage_with_pinned_objects<BeforePublish>(
        &self,
        repository: &Repository,
        persistent_object_database: &Odb<'_>,
        object_database: &Odb<'_>,
        _mempack: &Mempack<'_>,
        arguments: GitStageArguments,
        before_publish: BeforePublish,
    ) -> Result<StageResult, LocalGitFailure>
    where
        BeforePublish: FnOnce(),
    {
        let (mut index_lock, mut index) =
            IndexLock::acquire_for_repository(&self.repository_authority)?;
        validate_index_entry_count(&index)?;
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
                    if matches!(
                        source.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
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
        let mut written_objects = Vec::new();
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
                    written_objects.push(PackRoot::Object(oid));
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
        persist_objects(
            &self.repository_authority,
            repository,
            persistent_object_database,
            object_database,
            &written_objects,
        )?;
        index_lock.write(&mut index)?;
        drop(index);
        before_publish();
        self.validate_current_repository_identity()?;
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

    fn validate_current_repository_identity(&self) -> Result<(), LocalGitFailure> {
        let root =
            fs::symlink_metadata(&self.root_path).map_err(|_| LocalGitFailure::Repository)?;
        let git_directory = fs::symlink_metadata(self.root_path.join(".git"))
            .map_err(|_| LocalGitFailure::Repository)?;
        if root.file_type().is_symlink()
            || !root.is_dir()
            || git_directory.file_type().is_symlink()
            || !git_directory.is_dir()
        {
            return Err(LocalGitFailure::Repository);
        }
        let observed = RepositoryIdentity {
            root: file_identity(&root),
            git_directory: file_identity(&git_directory),
        };
        if observed == self.repository_identity {
            Ok(())
        } else {
            Err(LocalGitFailure::Repository)
        }
    }

    fn bind_locked_index(&self, repository: &Repository) -> Result<IndexLock, LocalGitFailure> {
        let (index_lock, mut index) =
            IndexLock::acquire_for_repository(&self.repository_authority)?;
        repository
            .set_index(&mut index)
            .map_err(|_| LocalGitFailure::Operation)?;
        Ok(index_lock)
    }

    fn bind_index_snapshot(
        &self,
        repository: &Repository,
    ) -> Result<IndexSnapshot, LocalGitFailure> {
        let (snapshot, mut index) =
            IndexSnapshot::acquire(&self.repository_authority.git_path("index"))?;
        repository
            .set_index(&mut index)
            .map_err(|_| LocalGitFailure::Operation)?;
        Ok(snapshot)
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
        let tracked_paths = index
            .iter()
            .map(|entry| PathBuf::from(OsString::from_vec(entry.path)))
            .collect::<BTreeSet<_>>();
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
                        if !tracked_paths.contains(&entry.path) {
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
        self.branch_switch_with_hooks(repository, arguments, || {}, || {}, || {})
    }

    #[cfg(test)]
    fn branch_switch_with_hook<Hook: FnOnce()>(
        &self,
        repository: &Repository,
        arguments: GitBranchSwitchArguments,
        post_checkout: Hook,
    ) -> Result<BranchResult, LocalGitFailure> {
        self.branch_switch_with_hooks(repository, arguments, || {}, post_checkout, || {})
    }

    #[cfg(test)]
    fn branch_switch_with_reference_lock_hook<Hook: FnOnce()>(
        &self,
        repository: &Repository,
        arguments: GitBranchSwitchArguments,
        before_reference_locks: Hook,
    ) -> Result<BranchResult, LocalGitFailure> {
        self.branch_switch_with_hooks(repository, arguments, before_reference_locks, || {}, || {})
    }

    #[cfg(test)]
    fn branch_switch_with_index_publish_hook<Hook: FnOnce()>(
        &self,
        repository: &Repository,
        arguments: GitBranchSwitchArguments,
        post_index_publish: Hook,
    ) -> Result<BranchResult, LocalGitFailure> {
        self.branch_switch_with_hooks(repository, arguments, || {}, || {}, post_index_publish)
    }

    fn branch_switch_with_hooks<
        BeforeLocks: FnOnce(),
        PostCheckout: FnOnce(),
        PostIndexPublish: FnOnce(),
    >(
        &self,
        repository: &Repository,
        arguments: GitBranchSwitchArguments,
        before_reference_locks: BeforeLocks,
        post_checkout: PostCheckout,
        post_index_publish: PostIndexPublish,
    ) -> Result<BranchResult, LocalGitFailure> {
        let mut index_lock = self.bind_locked_index(repository)?;
        if repository.state() != RepositoryState::Clean {
            return Err(LocalGitFailure::Operation);
        }
        let reference_name = format!("refs/heads/{}", arguments.name);
        if !git2::Reference::is_valid_name(&reference_name) {
            return Err(LocalGitFailure::Operation);
        }
        let (current_chain, initial_current) =
            resolve_pinned_reference_chain_from(&self.repository_authority, "HEAD", None)?;
        let (reference_chain, initial_target) =
            resolve_pinned_reference_chain_from(&self.repository_authority, &reference_name, None)?;
        let initial_target = initial_target.ok_or(LocalGitFailure::Operation)?;
        before_reference_locks();
        let lock_names = current_chain
            .iter()
            .chain(reference_chain.iter())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut reference_locks = lock_names
            .iter()
            .map(|reference| ReferenceLock::acquire(&self.repository_authority, reference))
            .collect::<Result<Vec<_>, _>>()?;
        let (locked_current_chain, current_target) = resolve_pinned_reference_chain_from(
            &self.repository_authority,
            "HEAD",
            Some(&reference_locks),
        )?;
        let (locked_chain, target) = resolve_pinned_reference_chain_from(
            &self.repository_authority,
            &reference_name,
            Some(&reference_locks),
        )?;
        if locked_current_chain != current_chain
            || current_target != initial_current
            || locked_chain != reference_chain
            || target != Some(initial_target)
        {
            return Err(LocalGitFailure::Operation);
        }
        let signature = self
            .identity
            .signature()
            .map_err(|_| LocalGitFailure::Operation)?;
        let target = initial_target;
        let target_commit = find_bounded_commit(repository, target)?;
        let current_tree = current_target
            .map(|current| tree_for_commit(repository, current))
            .transpose()?;
        let target_tree = find_bounded_tree(repository, target_commit.tree_id())?;
        if let Some(current_tree) = &current_tree {
            validate_checkout_tree_discovery(repository, current_tree)?;
        }
        validate_checkout_tree_discovery(repository, &target_tree)?;
        let mut current_index = repository.index().map_err(|_| LocalGitFailure::Operation)?;
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
        let changes = repository
            .diff_tree_to_tree(current_tree.as_ref(), Some(&target_tree), None)
            .map_err(|_| LocalGitFailure::Operation)?;
        let checkout_paths = changes
            .deltas()
            .filter_map(|delta| {
                delta
                    .new_file()
                    .path()
                    .or_else(|| delta.old_file().path())
                    .map(Path::to_owned)
            })
            .collect::<BTreeSet<_>>();
        if !staged_paths.is_disjoint(&checkout_paths) {
            return Err(LocalGitFailure::Operation);
        }
        let staged_entries = staged_paths
            .into_iter()
            .map(|path| {
                let entry = current_index
                    .get_path(&path, 0)
                    .map(|entry| clone_index_entry(&entry));
                (path, entry)
            })
            .collect::<Vec<_>>();
        for path in &checkout_paths {
            validate_checkout_path(
                &self.filesystem,
                &self.root,
                path,
                &current_index,
                &target_tree,
            )?;
        }
        let mut next_index = Index::new().map_err(|_| LocalGitFailure::Operation)?;
        next_index
            .read_tree(&target_tree)
            .map_err(|_| LocalGitFailure::Operation)?;
        for current_entry in current_index
            .iter()
            .filter(|entry| entry.flags & 0x3000 == 0)
        {
            let path = PathBuf::from(OsString::from_vec(current_entry.path.clone()));
            if checkout_paths.contains(&path) {
                continue;
            }
            if let Some(target_entry) = next_index.get_path(&path, 0) {
                let mut target_entry = clone_index_entry(&target_entry);
                target_entry.flags = current_entry.flags;
                target_entry.flags_extended = current_entry.flags_extended;
                next_index
                    .add(&target_entry)
                    .map_err(|_| LocalGitFailure::Operation)?;
            }
        }
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
        index_lock.write(&mut next_index)?;
        let checkout_started = Cell::new(false);
        let mut checkout = CheckoutBuilder::new();
        checkout
            .safe()
            .update_index(false)
            .refresh(false)
            .disable_filters(true);
        checkout
            .notify_on(CheckoutNotificationType::UPDATED)
            .notify(|_, _, _, _, _| {
                checkout_started.set(true);
                true
            });
        checkout_tree_with_rollback(
            repository,
            current_tree.as_ref(),
            &checkout_paths,
            &checkout_started,
            || {
                repository
                    .checkout_tree(target_commit.as_object(), Some(&mut checkout))
                    .map_err(|_| LocalGitFailure::Operation)
            },
        )?;
        post_checkout();
        let published_index_identity = match index_lock.commit() {
            Ok(identity) => identity,
            Err(_) => {
                rollback_checkout(repository, current_tree.as_ref(), &checkout_paths)?;
                return Err(LocalGitFailure::Operation);
            }
        };
        post_index_publish();
        let publish_result = (|| {
            let (current_chain_before_publish, current_before_publish) =
                resolve_pinned_reference_chain_from(
                    &self.repository_authority,
                    "HEAD",
                    Some(&reference_locks),
                )?;
            let (target_chain_before_publish, target_before_publish) =
                resolve_pinned_reference_chain_from(
                    &self.repository_authority,
                    &reference_name,
                    Some(&reference_locks),
                )?;
            if current_chain_before_publish != locked_current_chain
                || current_before_publish != current_target
                || target_chain_before_publish != locked_chain
                || target_before_publish != Some(target)
            {
                return Err(LocalGitFailure::Operation);
            }
            let head_lock = reference_locks
                .iter()
                .position(|lock| lock.name == "HEAD")
                .map(|position| reference_locks.swap_remove(position))
                .ok_or(LocalGitFailure::Operation)?;
            self.validate_current_repository()?;
            publish_symbolic_head(
                &self.repository_authority,
                head_lock,
                &reference_name,
                current_target.unwrap_or(git2::Oid::ZERO_SHA1),
                target,
                &signature,
            )
        })();
        if publish_result.is_err() {
            rollback_checkout(repository, current_tree.as_ref(), &checkout_paths)?;
            restore_index(
                &self.repository_authority,
                &mut current_index,
                published_index_identity,
            )?;
            return Err(LocalGitFailure::Operation);
        }
        Ok(BranchResult {
            branch: arguments.name,
            head: target.to_string(),
        })
    }
}

fn checkout_tree_with_rollback<Checkout: FnOnce() -> Result<(), LocalGitFailure>>(
    repository: &Repository,
    current_tree: Option<&git2::Tree<'_>>,
    checkout_paths: &BTreeSet<PathBuf>,
    checkout_started: &Cell<bool>,
    checkout: Checkout,
) -> Result<(), LocalGitFailure> {
    if checkout().is_err() {
        if checkout_started.get() {
            rollback_checkout(repository, current_tree, checkout_paths)?;
        }
        return Err(LocalGitFailure::Operation);
    }
    Ok(())
}

fn rollback_checkout(
    repository: &Repository,
    current_tree: Option<&git2::Tree<'_>>,
    checkout_paths: &BTreeSet<PathBuf>,
) -> Result<(), LocalGitFailure> {
    let generated_tree;
    let tree = if let Some(current_tree) = current_tree {
        current_tree
    } else {
        let mut empty = Index::new().map_err(|_| LocalGitFailure::Operation)?;
        let oid = empty
            .write_tree_to(repository)
            .map_err(|_| LocalGitFailure::Operation)?;
        generated_tree = repository
            .find_tree(oid)
            .map_err(|_| LocalGitFailure::Operation)?;
        &generated_tree
    };
    let mut checkout = CheckoutBuilder::new();
    checkout
        .force()
        .remove_untracked(true)
        .update_index(false)
        .refresh(false)
        .disable_filters(true);
    for path in checkout_paths {
        checkout.path(path);
    }
    repository
        .checkout_tree(tree.as_object(), Some(&mut checkout))
        .map_err(|_| LocalGitFailure::Operation)
}

fn restore_index(
    authority: &PinnedRepository,
    index: &mut Index,
    expected_identity: FileIdentity,
) -> Result<(), LocalGitFailure> {
    let (mut lock, _replacement) = IndexLock::acquire_for_repository(authority)?;
    let current_identity = fs::symlink_metadata(authority.git_path("index"))
        .map(|metadata| file_identity(&metadata))
        .map_err(|_| LocalGitFailure::Operation)?;
    if current_identity != expected_identity {
        return Err(LocalGitFailure::Operation);
    }
    lock.write(index)?;
    lock.commit().map(|_| ())
}

fn encode_result(result: &LocalGitResult) -> Result<String, LocalGitFailure> {
    let encoded = serde_json::to_string(result).map_err(|_| LocalGitFailure::Encoding)?;
    ToolResultText::try_new(encoded)
        .map(ToolResultText::into_string)
        .map_err(|_| LocalGitFailure::Encoding)
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PinnedReferenceValue {
    Direct(git2::Oid),
    Symbolic(String),
    Missing,
}

struct ReferenceLock {
    name: String,
    parent: OwnedFd,
    leaf: OsString,
    lock_name: OsString,
    lock: fs::File,
    identity: FileIdentity,
    hierarchy: Vec<(PathBuf, FileIdentity)>,
    _created_directories: CreatedReferenceDirectories,
    committed: bool,
}

struct ReferenceParent {
    directory: OwnedFd,
    leaf: OsString,
    hierarchy: Vec<(PathBuf, FileIdentity)>,
    created_directories: CreatedReferenceDirectories,
    creation_file_mode: Option<Mode>,
}

struct CreatedReferenceDirectory {
    parent: OwnedFd,
    name: OsString,
    identity: FileIdentity,
}

#[derive(Default)]
struct CreatedReferenceDirectories(Vec<CreatedReferenceDirectory>);

impl ReferenceLock {
    fn acquire(authority: &PinnedRepository, name: &str) -> Result<Self, LocalGitFailure> {
        let bound = open_reference_parent(authority, name, true)?;
        let creation_file_mode = bound.creation_file_mode;
        let parent = bound.directory;
        let leaf = bound.leaf;
        let mut lock_name = OsString::from(&leaf);
        lock_name.push(".lock");
        let descriptor = openat(
            &parent,
            &lock_name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        let lock = fs::File::from(descriptor);
        let identity = file_identity(&lock.metadata().map_err(|_| LocalGitFailure::Operation)?);
        let guard = Self {
            name: name.to_owned(),
            parent,
            leaf,
            lock_name,
            lock,
            identity,
            hierarchy: bound.hierarchy,
            _created_directories: bound.created_directories,
            committed: false,
        };
        let permissions = reference_permissions(&guard.parent, &guard.leaf)?
            .or_else(|| creation_file_mode.map(|mode| fs::Permissions::from_mode(mode.bits())));
        if let Some(permissions) = permissions {
            guard
                .lock
                .set_permissions(permissions)
                .map_err(|_| LocalGitFailure::Operation)?;
        }
        Ok(guard)
    }

    fn read(&self, authority: &PinnedRepository) -> Result<PinnedReferenceValue, LocalGitFailure> {
        read_reference_leaf(&self.parent, &self.leaf, authority, &self.name)
    }

    fn hierarchy_is_current(&self, authority: &PinnedRepository) -> bool {
        self.hierarchy.iter().all(|(relative, expected)| {
            open_git_directory_path(authority, relative)
                .and_then(|directory| {
                    let metadata = fs::File::from(directory)
                        .metadata()
                        .map_err(|_| LocalGitFailure::Operation)?;
                    Ok(file_identity(&metadata) == *expected)
                })
                .unwrap_or(false)
        })
    }

    fn prepare(
        &mut self,
        authority: &PinnedRepository,
        target: git2::Oid,
    ) -> Result<(), LocalGitFailure> {
        writeln!(self.lock, "{target}").map_err(|_| LocalGitFailure::Operation)?;
        self.lock
            .sync_all()
            .map_err(|_| LocalGitFailure::Operation)?;
        if !self.path_still_owned() || !self.hierarchy_is_current(authority) {
            return Err(LocalGitFailure::Operation);
        }
        Ok(())
    }

    fn prepare_symbolic(
        &mut self,
        authority: &PinnedRepository,
        target: &str,
    ) -> Result<(), LocalGitFailure> {
        writeln!(self.lock, "ref: {target}").map_err(|_| LocalGitFailure::Operation)?;
        self.lock
            .sync_all()
            .map_err(|_| LocalGitFailure::Operation)?;
        if !self.path_still_owned() || !self.hierarchy_is_current(authority) {
            return Err(LocalGitFailure::Operation);
        }
        Ok(())
    }

    fn publish(mut self, authority: &PinnedRepository) -> Result<(), LocalGitFailure> {
        if !self.path_still_owned() || !self.hierarchy_is_current(authority) {
            return Err(LocalGitFailure::Operation);
        }
        renameat_with(
            &self.parent,
            &self.lock_name,
            &self.parent,
            &self.leaf,
            RenameFlags::empty(),
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        self.committed = true;
        Ok(())
    }

    #[cfg(test)]
    fn commit(
        mut self,
        authority: &PinnedRepository,
        target: git2::Oid,
    ) -> Result<(), LocalGitFailure> {
        self.prepare(authority, target)?;
        self.publish(authority)
    }

    fn path_still_owned(&self) -> bool {
        let descriptor_identity = self
            .lock
            .metadata()
            .map(|metadata| file_identity(&metadata))
            .ok();
        let path_identity = openat(
            &self.parent,
            &self.lock_name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()
        .and_then(|descriptor| fs::File::from(descriptor).metadata().ok())
        .map(|metadata| file_identity(&metadata));
        descriptor_identity == Some(self.identity) && path_identity == Some(self.identity)
    }
}

fn reference_permissions(
    parent: &OwnedFd,
    leaf: &OsStr,
) -> Result<Option<fs::Permissions>, LocalGitFailure> {
    let descriptor = match openat(
        parent,
        leaf,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
        Err(_) => return Err(LocalGitFailure::Operation),
    };
    let metadata = fs::File::from(descriptor)
        .metadata()
        .map_err(|_| LocalGitFailure::Operation)?;
    if !metadata.is_file() {
        return Err(LocalGitFailure::Operation);
    }
    Ok(Some(fs::Permissions::from_mode(metadata.mode() & 0o777)))
}

impl Drop for ReferenceLock {
    fn drop(&mut self) {
        if !self.committed && self.path_still_owned() {
            let _ = unlinkat(&self.parent, &self.lock_name, AtFlags::empty());
        }
    }
}

impl Drop for CreatedReferenceDirectories {
    fn drop(&mut self) {
        for directory in self.0.iter().rev() {
            directory.remove_if_owned();
        }
    }
}

impl CreatedReferenceDirectory {
    fn remove_if_owned(&self) {
        let current_identity = openat(
            &self.parent,
            &self.name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()
        .and_then(|directory| fs::File::from(directory).metadata().ok())
        .map(|metadata| file_identity(&metadata));
        if current_identity == Some(self.identity) {
            let _ = unlinkat(&self.parent, &self.name, AtFlags::REMOVEDIR);
        }
    }
}

fn open_reference_parent(
    authority: &PinnedRepository,
    name: &str,
    create: bool,
) -> Result<ReferenceParent, LocalGitFailure> {
    if name != "HEAD" && (!name.starts_with("refs/") || !git2::Reference::is_valid_name(name)) {
        return Err(LocalGitFailure::Operation);
    }
    let path = Path::new(name);
    let leaf = path
        .file_name()
        .filter(|leaf| !leaf.is_empty())
        .ok_or(LocalGitFailure::Operation)?
        .to_owned();
    let parent_path = path.parent().unwrap_or_else(|| Path::new(""));
    let creation_modes = if create && name.starts_with("refs/") {
        Some(reference_creation_modes(authority)?)
    } else {
        None
    };
    let mut directory = dup(&authority.git_directory).map_err(|_| LocalGitFailure::Operation)?;
    let mut relative = PathBuf::new();
    let mut hierarchy = vec![(
        relative.clone(),
        file_identity(
            &fs::File::from(dup(&directory).map_err(|_| LocalGitFailure::Operation)?)
                .metadata()
                .map_err(|_| LocalGitFailure::Operation)?,
        ),
    )];
    let mut created_directories = CreatedReferenceDirectories::default();
    for component in parent_path.components() {
        let Component::Normal(component) = component else {
            return Err(LocalGitFailure::Operation);
        };
        let parent = dup(&directory).map_err(|_| LocalGitFailure::Operation)?;
        let (next_directory, created) = if create {
            match creation_modes {
                Some((directory_mode, _)) => open_or_create_ref_directory_with_mode_tracked(
                    &directory,
                    component,
                    directory_mode,
                )?,
                None => (open_or_create_ref_directory(&directory, component)?, false),
            }
        } else {
            (
                openat(
                    &directory,
                    component,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|_| LocalGitFailure::Operation)?,
                false,
            )
        };
        directory = next_directory;
        relative.push(component);
        let identity = file_identity(
            &fs::File::from(dup(&directory).map_err(|_| LocalGitFailure::Operation)?)
                .metadata()
                .map_err(|_| LocalGitFailure::Operation)?,
        );
        if created {
            created_directories.0.push(CreatedReferenceDirectory {
                parent,
                name: component.to_owned(),
                identity,
            });
        }
        hierarchy.push((relative.clone(), identity));
    }
    Ok(ReferenceParent {
        directory,
        leaf,
        hierarchy,
        created_directories,
        creation_file_mode: creation_modes.map(|(_, file_mode)| file_mode),
    })
}

fn reference_creation_modes(authority: &PinnedRepository) -> Result<(Mode, Mode), LocalGitFailure> {
    let refs = openat(
        &authority.git_directory,
        "refs",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitFailure::Operation)?;
    reference_installation_modes(&refs)
}

fn open_git_directory_path(
    authority: &PinnedRepository,
    relative: &Path,
) -> Result<OwnedFd, LocalGitFailure> {
    let mut directory = dup(&authority.git_directory).map_err(|_| LocalGitFailure::Operation)?;
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(LocalGitFailure::Operation);
        };
        directory = openat(
            &directory,
            component,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Operation)?;
    }
    Ok(directory)
}

fn read_pinned_reference(
    authority: &PinnedRepository,
    name: &str,
) -> Result<PinnedReferenceValue, LocalGitFailure> {
    let bound = match open_reference_parent(authority, name, false) {
        Ok(bound) => bound,
        Err(LocalGitFailure::Operation) if name.starts_with("refs/") => {
            return packed_reference_target(authority, name).map(|target| {
                target.map_or(PinnedReferenceValue::Missing, PinnedReferenceValue::Direct)
            });
        }
        Err(error) => return Err(error),
    };
    read_reference_leaf(&bound.directory, &bound.leaf, authority, name)
}

fn read_reference_leaf(
    parent: &OwnedFd,
    leaf: &OsStr,
    authority: &PinnedRepository,
    name: &str,
) -> Result<PinnedReferenceValue, LocalGitFailure> {
    let descriptor = match openat(
        parent,
        leaf,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) if error == rustix::io::Errno::NOENT && name.starts_with("refs/") => {
            return packed_reference_target(authority, name).map(|target| {
                target.map_or(PinnedReferenceValue::Missing, PinnedReferenceValue::Direct)
            });
        }
        Err(error) if error == rustix::io::Errno::NOENT => {
            return Ok(PinnedReferenceValue::Missing);
        }
        Err(_) => return Err(LocalGitFailure::Operation),
    };
    let mut file = fs::File::from(descriptor);
    let metadata = file.metadata().map_err(|_| LocalGitFailure::Operation)?;
    if !metadata.is_file() || metadata.len() > MAX_REVISION_BYTES as u64 {
        return Err(LocalGitFailure::Operation);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take((MAX_REVISION_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalGitFailure::Operation)?;
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    if let Some(symbolic) = bytes.strip_prefix(b"ref: ") {
        let symbolic = std::str::from_utf8(symbolic).map_err(|_| LocalGitFailure::Operation)?;
        if !symbolic.starts_with("refs/") || !git2::Reference::is_valid_name(symbolic) {
            return Err(LocalGitFailure::Operation);
        }
        return Ok(PinnedReferenceValue::Symbolic(symbolic.to_owned()));
    }
    let direct = std::str::from_utf8(bytes)
        .ok()
        .and_then(|value| git2::Oid::from_str(value).ok())
        .ok_or(LocalGitFailure::Operation)?;
    Ok(PinnedReferenceValue::Direct(direct))
}

fn resolve_pinned_reference_chain(
    authority: &PinnedRepository,
    locks: Option<&[ReferenceLock]>,
) -> Result<(Vec<String>, Option<git2::Oid>), LocalGitFailure> {
    resolve_pinned_reference_chain_from(authority, "HEAD", locks)
}

fn resolve_pinned_reference_chain_from(
    authority: &PinnedRepository,
    start: &str,
    locks: Option<&[ReferenceLock]>,
) -> Result<(Vec<String>, Option<git2::Oid>), LocalGitFailure> {
    const MAX_SYMBOLIC_REFERENCE_DEPTH: usize = 16;
    let mut names = Vec::new();
    let mut current = start.to_owned();
    loop {
        if names.len() == MAX_SYMBOLIC_REFERENCE_DEPTH || names.contains(&current) {
            return Err(LocalGitFailure::Operation);
        }
        let value = match locks {
            Some(locks) => locks
                .iter()
                .find(|lock| lock.name == current)
                .ok_or(LocalGitFailure::Operation)?
                .read(authority)?,
            None => read_pinned_reference(authority, &current)?,
        };
        names.push(current);
        match value {
            PinnedReferenceValue::Direct(oid) => return Ok((names, Some(oid))),
            PinnedReferenceValue::Symbolic(target) => current = target,
            PinnedReferenceValue::Missing => return Ok((names, None)),
        }
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
    _private_directory: tempfile::TempDir,
    private_index_path: PathBuf,
    committed: bool,
}

struct IndexSnapshot {
    _file: fs::File,
}

impl IndexSnapshot {
    fn acquire(index_path: &Path) -> Result<(Self, Index), LocalGitFailure> {
        let mut file = tempfile::tempfile().map_err(|_| LocalGitFailure::Operation)?;
        copy_index_snapshot(index_path, &mut file, false)?;
        let index = Index::open(&descriptor_path(&file)).map_err(|_| LocalGitFailure::Operation)?;
        Ok((Self { _file: file }, index))
    }
}

impl IndexLock {
    fn acquire_for_repository(
        authority: &PinnedRepository,
    ) -> Result<(Self, Index), LocalGitFailure> {
        Self::acquire_with_private_directory_and_mode(
            &authority.git_path("index"),
            &authority.git_path("index.lock"),
            index_installation_mode(authority)?,
            tempfile::tempdir,
        )
    }

    #[cfg(test)]
    fn acquire(index_path: &Path, lock_path: &Path) -> Result<(Self, Index), LocalGitFailure> {
        Self::acquire_with_private_directory_and_mode(
            index_path,
            lock_path,
            Mode::RUSR | Mode::WUSR,
            tempfile::tempdir,
        )
    }

    #[cfg(test)]
    fn acquire_with_private_directory<Create>(
        index_path: &Path,
        lock_path: &Path,
        create_private_directory: Create,
    ) -> Result<(Self, Index), LocalGitFailure>
    where
        Create: FnOnce() -> std::io::Result<tempfile::TempDir>,
    {
        Self::acquire_with_private_directory_and_mode(
            index_path,
            lock_path,
            Mode::RUSR | Mode::WUSR,
            create_private_directory,
        )
    }

    fn acquire_with_private_directory_and_mode<Create>(
        index_path: &Path,
        lock_path: &Path,
        missing_index_mode: Mode,
        create_private_directory: Create,
    ) -> Result<(Self, Index), LocalGitFailure>
    where
        Create: FnOnce() -> std::io::Result<tempfile::TempDir>,
    {
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(lock_path)
            .map_err(|_| LocalGitFailure::Operation)?;
        let identity = file_identity(&lock.metadata().map_err(|_| LocalGitFailure::Operation)?);
        let private_directory = match create_private_directory() {
            Ok(directory) => directory,
            Err(_) => {
                remove_owned_index_lock(lock_path, &lock, identity);
                return Err(LocalGitFailure::Operation);
            }
        };
        let private_index_path = private_directory.path().join("index");
        let mut guard = Self {
            index_path: index_path.to_owned(),
            lock_path: lock_path.to_owned(),
            lock,
            identity,
            _private_directory: private_directory,
            private_index_path,
            committed: false,
        };
        guard
            .lock
            .set_permissions(fs::Permissions::from_mode(missing_index_mode.bits()))
            .map_err(|_| LocalGitFailure::Operation)?;
        copy_index_snapshot(index_path, &mut guard.lock, true)?;
        guard
            .lock
            .sync_all()
            .map_err(|_| LocalGitFailure::Operation)?;
        guard.copy_lock_to_private_index()?;
        let index =
            Index::open(&guard.private_index_path).map_err(|_| LocalGitFailure::Operation)?;
        Ok((guard, index))
    }

    fn copy_lock_to_private_index(&mut self) -> Result<(), LocalGitFailure> {
        let mut private_index = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&self.private_index_path)
            .map_err(|_| LocalGitFailure::Operation)?;
        self.lock.rewind().map_err(|_| LocalGitFailure::Operation)?;
        let copied = std::io::copy(
            &mut Read::by_ref(&mut self.lock).take((MAX_INDEX_BYTES + 1) as u64),
            &mut private_index,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        if copied > MAX_INDEX_BYTES as u64 {
            return Err(LocalGitFailure::Operation);
        }
        private_index
            .sync_all()
            .map_err(|_| LocalGitFailure::Operation)
    }

    fn write(&mut self, index: &mut Index) -> Result<(), LocalGitFailure> {
        if index.path() != Some(self.private_index_path.as_path()) {
            return write_index_entries(&mut self.lock, index);
        }
        index.write().map_err(|_| LocalGitFailure::Operation)?;
        let descriptor = openat(
            CWD,
            &self.private_index_path,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        let mut source = fs::File::from(descriptor);
        let metadata = source.metadata().map_err(|_| LocalGitFailure::Operation)?;
        if !metadata.is_file() || metadata.len() > MAX_INDEX_BYTES as u64 {
            return Err(LocalGitFailure::Operation);
        }
        self.lock
            .set_len(0)
            .map_err(|_| LocalGitFailure::Operation)?;
        self.lock.rewind().map_err(|_| LocalGitFailure::Operation)?;
        let copied = std::io::copy(
            &mut Read::by_ref(&mut source).take((MAX_INDEX_BYTES + 1) as u64),
            &mut self.lock,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        if copied != metadata.len() || copied > MAX_INDEX_BYTES as u64 {
            return Err(LocalGitFailure::Operation);
        }
        self.lock.sync_all().map_err(|_| LocalGitFailure::Operation)
    }

    fn commit(mut self) -> Result<FileIdentity, LocalGitFailure> {
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
        Ok(self.identity)
    }
}

fn index_installation_mode(authority: &PinnedRepository) -> Result<Mode, LocalGitFailure> {
    let metadata =
        fs::File::from(dup(&authority.git_directory).map_err(|_| LocalGitFailure::Operation)?)
            .metadata()
            .map_err(|_| LocalGitFailure::Operation)?;
    Ok(Mode::from_raw_mode((metadata.mode() & 0o666) | 0o600))
}

fn write_index_entries(destination: &mut fs::File, index: &Index) -> Result<(), LocalGitFailure> {
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
    destination
        .set_len(0)
        .map_err(|_| LocalGitFailure::Operation)?;
    destination
        .rewind()
        .map_err(|_| LocalGitFailure::Operation)?;
    destination
        .write_all(&bytes)
        .and_then(|()| destination.sync_all())
        .map_err(|_| LocalGitFailure::Operation)
}

fn copy_index_snapshot(
    index_path: &Path,
    destination: &mut fs::File,
    preserve_permissions: bool,
) -> Result<(), LocalGitFailure> {
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
            if preserve_permissions {
                destination
                    .set_permissions(metadata.permissions())
                    .map_err(|_| LocalGitFailure::Operation)?;
            }
            let copied = std::io::copy(
                &mut Read::by_ref(&mut source).take((MAX_INDEX_BYTES + 1) as u64),
                destination,
            )
            .map_err(|_| LocalGitFailure::Operation)?;
            if copied > MAX_INDEX_BYTES as u64 {
                return Err(LocalGitFailure::Repository);
            }
        }
        Err(rustix::io::Errno::NOENT) => write_empty_index(destination)?,
        Err(_) => return Err(LocalGitFailure::Repository),
    }
    Ok(())
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        if !self.committed {
            remove_owned_index_lock(&self.lock_path, &self.lock, self.identity);
        }
    }
}

fn remove_owned_index_lock(lock_path: &Path, lock: &fs::File, identity: FileIdentity) {
    let path_identity = fs::symlink_metadata(lock_path)
        .map(|metadata| file_identity(&metadata))
        .ok();
    let descriptor_identity = lock
        .metadata()
        .map(|metadata| file_identity(&metadata))
        .ok();
    if path_identity == Some(identity) && descriptor_identity == Some(identity) {
        let _ = fs::remove_file(lock_path);
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
    current_index: &Index,
    target_tree: &git2::Tree<'_>,
) -> Result<(), LocalGitFailure> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    match filesystem.entry_kind(root, parent) {
        Ok(WorkspaceEntryKind::Directory) => {}
        Ok(_)
            if current_index.get_path(parent, 0).is_some()
                && target_tree
                    .get_path(parent)
                    .is_ok_and(|entry| entry.kind() == Some(git2::ObjectType::Tree)) => {}
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

fn status_head(
    authority: &PinnedRepository,
) -> Result<(Option<String>, bool, Option<git2::Oid>), LocalGitFailure> {
    let value = read_status_reference(authority, b"HEAD")?;
    let StatusReferenceValue::Symbolic(target) = value else {
        return match value {
            StatusReferenceValue::Direct(target) => Ok((None, false, Some(target))),
            StatusReferenceValue::Missing | StatusReferenceValue::Symbolic(_) => {
                Err(LocalGitFailure::Operation)
            }
        };
    };
    let branch = target.strip_prefix(b"refs/heads/");
    let (branch, branch_truncated) = match branch {
        Some(branch) => {
            let (branch, truncated) = bounded_bytes(branch, MAX_BRANCH_BYTES);
            (Some(branch), truncated)
        }
        None => (None, false),
    };
    let target = resolve_status_reference_chain(authority, target)?;
    Ok((branch, branch_truncated, target))
}

#[cfg(test)]
fn status_head_from_reference(
    head: &git2::Reference<'_>,
) -> Result<(Option<String>, bool, Option<git2::Oid>), LocalGitFailure> {
    let branch = head
        .symbolic_target_bytes()
        .and_then(|target| target.strip_prefix(b"refs/heads/"));
    let (branch, branch_truncated) = match branch {
        Some(branch) => {
            let (branch, truncated) = bounded_bytes(branch, MAX_BRANCH_BYTES);
            (Some(branch), truncated)
        }
        None => (None, false),
    };
    let target = match head.target() {
        Some(target) => Some(target),
        None => match head.resolve() {
            Ok(resolved) => Some(resolved.target().ok_or(LocalGitFailure::Operation)?),
            Err(error) if error.code() == ErrorCode::NotFound => None,
            Err(_) => return Err(LocalGitFailure::Operation),
        },
    };
    Ok((branch, branch_truncated, target))
}

enum StatusReferenceValue {
    Direct(git2::Oid),
    Symbolic(Vec<u8>),
    Missing,
}

fn resolve_status_reference_chain(
    authority: &PinnedRepository,
    start: Vec<u8>,
) -> Result<Option<git2::Oid>, LocalGitFailure> {
    const MAX_SYMBOLIC_REFERENCE_DEPTH: usize = 16;
    let mut names = Vec::new();
    let mut current = start;
    loop {
        if names.len() == MAX_SYMBOLIC_REFERENCE_DEPTH || names.contains(&current) {
            return Err(LocalGitFailure::Operation);
        }
        let value = read_status_reference(authority, &current)?;
        names.push(current);
        match value {
            StatusReferenceValue::Direct(oid) => return Ok(Some(oid)),
            StatusReferenceValue::Symbolic(target) => current = target,
            StatusReferenceValue::Missing => return Ok(None),
        }
    }
}

fn read_status_reference(
    authority: &PinnedRepository,
    name: &[u8],
) -> Result<StatusReferenceValue, LocalGitFailure> {
    let Some((parent, leaf)) = open_status_reference_parent(authority, name)? else {
        return status_packed_reference(authority, name);
    };
    let descriptor = match openat(
        &parent,
        &leaf,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) if error == rustix::io::Errno::NOENT => {
            return status_packed_reference(authority, name);
        }
        Err(_) => return Err(LocalGitFailure::Operation),
    };
    let mut file = fs::File::from(descriptor);
    let metadata = file.metadata().map_err(|_| LocalGitFailure::Operation)?;
    if !metadata.is_file() || metadata.len() > MAX_REVISION_BYTES as u64 {
        return Err(LocalGitFailure::Operation);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take((MAX_REVISION_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalGitFailure::Operation)?;
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    if let Some(symbolic) = bytes.strip_prefix(b"ref: ") {
        status_reference_path(symbolic)?;
        return Ok(StatusReferenceValue::Symbolic(symbolic.to_vec()));
    }
    let direct = std::str::from_utf8(bytes)
        .ok()
        .and_then(|value| git2::Oid::from_str(value).ok())
        .ok_or(LocalGitFailure::Operation)?;
    Ok(StatusReferenceValue::Direct(direct))
}

fn open_status_reference_parent(
    authority: &PinnedRepository,
    name: &[u8],
) -> Result<Option<(OwnedFd, OsString)>, LocalGitFailure> {
    let path = status_reference_path(name)?;
    let leaf = path
        .file_name()
        .filter(|leaf| !leaf.is_empty())
        .ok_or(LocalGitFailure::Operation)?
        .to_owned();
    let mut directory = dup(&authority.git_directory).map_err(|_| LocalGitFailure::Operation)?;
    for component in path.parent().unwrap_or_else(|| Path::new("")).components() {
        let Component::Normal(component) = component else {
            return Err(LocalGitFailure::Operation);
        };
        directory = match openat(
            &directory,
            component,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(directory) => directory,
            Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
            Err(_) => return Err(LocalGitFailure::Operation),
        };
    }
    Ok(Some((directory, leaf)))
}

fn status_reference_path(name: &[u8]) -> Result<PathBuf, LocalGitFailure> {
    if name != b"HEAD" && !name.starts_with(b"refs/") {
        return Err(LocalGitFailure::Operation);
    }
    let path = PathBuf::from(OsString::from_vec(name.to_vec()));
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(LocalGitFailure::Operation);
    }
    Ok(path)
}

fn status_packed_reference(
    authority: &PinnedRepository,
    name: &[u8],
) -> Result<StatusReferenceValue, LocalGitFailure> {
    let Some(name) = std::str::from_utf8(name).ok() else {
        return Ok(StatusReferenceValue::Missing);
    };
    packed_reference_target(authority, name)
        .map(|target| target.map_or(StatusReferenceValue::Missing, StatusReferenceValue::Direct))
}

fn worktree_head_tree<'repository>(
    repository: &'repository Repository,
    authority: &PinnedRepository,
) -> Result<Option<git2::Tree<'repository>>, LocalGitFailure> {
    let (_, _, target) = status_head(authority)?;
    target
        .map(|target| tree_for_commit(repository, target))
        .transpose()
}

fn status<FileSystem: WorkspaceFileSystem>(
    repository: &Repository,
    authority: &PinnedRepository,
    filesystem: &FileSystem,
    root: &WorkspaceRoot,
    untracked: Vec<PathBuf>,
) -> Result<StatusResult, LocalGitFailure> {
    let (branch, branch_truncated, head_oid) = status_head(authority)?;
    let head = head_oid.map(|oid| oid.to_string());
    let head_tree = head_oid
        .map(|oid| tree_for_commit(repository, oid))
        .transpose()?;
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
            let tracked_in_head = head_tree.as_ref().is_some_and(|tree| {
                tree.get_path(path)
                    .is_ok_and(|entry| entry.filemode() == GITLINK_MODE as i32)
            });
            if !tracked_in_head {
                continue;
            }
            match filesystem.entry_kind(root, path) {
                Ok(WorkspaceEntryKind::Directory) => {}
                Err(WorkspaceResolveError::Io { source, .. })
                    if matches!(
                        source.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    set_worktree_status(&mut raw, path, "deleted");
                }
                Ok(WorkspaceEntryKind::File) => {
                    set_worktree_status(&mut raw, path, "type_changed");
                }
                Ok(WorkspaceEntryKind::Symlink | WorkspaceEntryKind::Other)
                | Err(WorkspaceResolveError::Rejected(_)) => {
                    return Err(LocalGitFailure::Path);
                }
                Err(WorkspaceResolveError::Io { .. }) => {
                    return Err(LocalGitFailure::Operation);
                }
            }
            continue;
        }
        if matches!(
            filesystem.entry_kind(root, path),
            Ok(WorkspaceEntryKind::Symlink)
                | Err(WorkspaceResolveError::Rejected(
                    WorkspacePathRejection::Symlink
                ))
        ) {
            let bytes = read_worktree_symlink(authority, path, MAX_OBJECT_BYTES)?;
            charge_worktree_bytes(&mut worktree_bytes, bytes.len())?;
            if *mode != 0o120000 {
                set_worktree_status(&mut raw, path, "type_changed");
            } else if blob_oid(&bytes)? != *oid {
                set_worktree_status(&mut raw, path, "modified");
            }
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
                if matches!(
                    source.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
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
        .filter(|entry| {
            entry.flags & 0x3000 == 0
                && entry.flags & INDEX_ASSUME_VALID == 0
                && entry.flags_extended & INDEX_SKIP_WORKTREE == 0
        })
        .map(|entry| {
            (
                PathBuf::from(std::ffi::OsString::from_vec(entry.path)),
                (entry.id, entry.mode),
            )
        })
        .collect()
}

fn index_backed_worktree_files(index: &Index) -> BTreeMap<PathBuf, (git2::Oid, u32)> {
    index
        .iter()
        .filter(|entry| {
            entry.flags & 0x3000 == 0
                && (entry.flags & INDEX_ASSUME_VALID != 0
                    || entry.flags_extended & INDEX_SKIP_WORKTREE != 0)
        })
        .map(|entry| {
            (
                PathBuf::from(std::ffi::OsString::from_vec(entry.path)),
                (entry.id, entry.mode),
            )
        })
        .collect()
}

fn conflicted_index_paths(index: &Index) -> BTreeSet<PathBuf> {
    index
        .iter()
        .filter(|entry| entry.flags & 0x3000 != 0)
        .map(|entry| PathBuf::from(OsString::from_vec(entry.path)))
        .collect()
}

fn tracked_directories(index: &Index) -> BTreeSet<PathBuf> {
    let mut directories = BTreeSet::new();
    for path in index
        .iter()
        .map(|entry| PathBuf::from(OsString::from_vec(entry.path)))
    {
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

fn diff<FileSystem: WorkspaceFileSystem>(
    repository: &Repository,
    authority: &PinnedRepository,
    arguments: GitDiffArguments,
    filesystem: &FileSystem,
    root: &WorkspaceRoot,
    untracked: Vec<PathBuf>,
) -> Result<DiffResult, LocalGitFailure> {
    let GitDiffArguments::Revisions { base, head } = arguments else {
        return worktree_diff(repository, authority, filesystem, root, untracked);
    };
    let mut options = DiffOptions::new();
    options.ignore_submodules(false);
    let base_tree = resolve_bounded_tree(repository, authority, &base)?;
    let head_tree = resolve_bounded_tree(repository, authority, &head)?;
    validate_tree_discovery(repository, &base_tree)?;
    validate_tree_discovery(repository, &head_tree)?;
    let diff = repository
        .diff_tree_to_tree(Some(&base_tree), Some(&head_tree), Some(&mut options))
        .map_err(|_| LocalGitFailure::Operation)?;
    render_diff(&diff)
}

fn worktree_diff<FileSystem: WorkspaceFileSystem>(
    repository: &Repository,
    authority: &PinnedRepository,
    filesystem: &FileSystem,
    root: &WorkspaceRoot,
    untracked: Vec<PathBuf>,
) -> Result<DiffResult, LocalGitFailure> {
    let head_tree = worktree_head_tree(repository, authority)?;
    let head_files = match head_tree.as_ref() {
        Some(tree) => {
            validate_tree_discovery(repository, tree)?;
            tree_files(repository, tree)?
        }
        None => BTreeMap::new(),
    };
    let index = repository.index().map_err(|_| LocalGitFailure::Operation)?;
    validate_index_objects(repository, &index)?;
    let index_files = index_files(&index);
    let index_backed_worktree_files = index_backed_worktree_files(&index);
    let conflicted_paths = conflicted_index_paths(&index);
    let mut diff_index_files = index_files.clone();
    diff_index_files.extend(index_backed_worktree_files.clone());
    let untracked_files = untracked
        .into_iter()
        .filter(|path| {
            matches!(
                filesystem.entry_kind(root, path),
                Ok(WorkspaceEntryKind::File)
            )
        })
        .collect::<BTreeSet<_>>();
    let mut bytes = Vec::new();
    let mut truncated = false;
    let filemode = repository_filemode(repository)?;
    let mut worktree_bytes = 0_usize;
    if truncated {
        return render_patch_bytes(bytes, true);
    }
    let paths = head_files
        .keys()
        .chain(diff_index_files.keys())
        .chain(conflicted_paths.iter())
        .chain(untracked_files.iter())
        .cloned()
        .collect::<BTreeSet<_>>();
    for path in paths {
        let old_buffer = match head_files.get(&path) {
            Some((oid, mode)) => Some((diff_object_buffer(repository, *oid, *mode)?, *mode)),
            None => None,
        };
        let new_buffer = if let Some((oid, mode)) = index_backed_worktree_files.get(&path) {
            Some((diff_object_buffer(repository, *oid, *mode)?, *mode))
        } else if diff_index_files
            .get(&path)
            .is_some_and(|(_, mode)| *mode == GITLINK_MODE)
            && !head_files.contains_key(&path)
        {
            let (oid, mode) = diff_index_files
                .get(&path)
                .ok_or(LocalGitFailure::Operation)?;
            Some((gitlink_buffer(*oid), *mode))
        } else if index_files.contains_key(&path)
            || conflicted_paths.contains(&path)
            || untracked_files.contains(&path)
        {
            match filesystem.entry_kind(root, &path) {
                Ok(WorkspaceEntryKind::Directory) => diff_index_files
                    .get(&path)
                    .filter(|(_, mode)| *mode == GITLINK_MODE)
                    .map(|(oid, mode)| (gitlink_buffer(*oid), *mode)),
                Ok(WorkspaceEntryKind::Symlink)
                | Err(WorkspaceResolveError::Rejected(WorkspacePathRejection::Symlink)) => {
                    let bytes = read_worktree_symlink(authority, &path, MAX_OBJECT_BYTES)?;
                    charge_worktree_bytes(&mut worktree_bytes, bytes.len())?;
                    Some((bytes, 0o120000))
                }
                Ok(WorkspaceEntryKind::Other) => return Err(LocalGitFailure::Path),
                Err(WorkspaceResolveError::Io { source, .. })
                    if matches!(
                        source.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
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
        let patch = match (old_buffer.as_ref(), new_buffer.as_ref()) {
            (Some((old, _old_mode)), Some((new, _new_mode))) => {
                Patch::from_buffers(old, Some(&path), new, Some(&path), Some(&mut options))
            }
            (Some((old, _mode)), None) => {
                Patch::from_buffers(old, Some(&path), b"", None, Some(&mut options))
            }
            (None, Some((new, _mode))) => {
                Patch::from_buffers(b"", None, new, Some(&path), Some(&mut options))
            }
            (None, None) => continue,
        }
        .map_err(|_| LocalGitFailure::Operation)?;
        let old_mode = head_files.get(&path).map(|(_, mode)| *mode);
        let new_mode = new_buffer.as_ref().map(|(_, mode)| *mode);
        append_bounded(&mut bytes, patch, &path, old_mode, new_mode, &mut truncated)?;
        if truncated {
            break;
        }
    }
    render_patch_bytes(bytes, truncated)
}

fn diff_object_buffer(
    repository: &Repository,
    oid: git2::Oid,
    mode: u32,
) -> Result<Vec<u8>, LocalGitFailure> {
    if mode == GITLINK_MODE {
        return Ok(gitlink_buffer(oid));
    }
    repository
        .find_blob(oid)
        .map(|blob| blob.content().to_vec())
        .map_err(|_| LocalGitFailure::Operation)
}

fn gitlink_buffer(oid: git2::Oid) -> Vec<u8> {
    format!("Subproject commit {oid}\n").into_bytes()
}

fn read_worktree_symlink(
    authority: &PinnedRepository,
    path: &Path,
    max_bytes: usize,
) -> Result<Vec<u8>, LocalGitFailure> {
    let leaf = path
        .file_name()
        .filter(|leaf| !leaf.is_empty())
        .ok_or(LocalGitFailure::Path)?;
    let mut directory = dup(&authority.root).map_err(|_| LocalGitFailure::Operation)?;
    for component in path.parent().unwrap_or_else(|| Path::new("")).components() {
        let Component::Normal(component) = component else {
            return Err(LocalGitFailure::Path);
        };
        directory = openat(
            &directory,
            component,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Path)?;
    }
    let mut buffer = vec![0_u8; max_bytes.saturating_add(1)];
    let length =
        readlinkat_raw(&directory, leaf, &mut buffer).map_err(|_| LocalGitFailure::Operation)?;
    if length > max_bytes {
        return Err(LocalGitFailure::Operation);
    }
    buffer.truncate(length);
    Ok(buffer)
}

fn append_bounded(
    bytes: &mut Vec<u8>,
    mut patch: Patch<'_>,
    path: &Path,
    old_mode: Option<u32>,
    new_mode: Option<u32>,
    truncated: &mut bool,
) -> Result<(), LocalGitFailure> {
    let patch = patch.to_buf().map_err(|_| LocalGitFailure::Operation)?;
    let patch = patch_with_modes(path, &patch, old_mode, new_mode)?;
    let remaining = MAX_DIFF_BYTES.saturating_sub(bytes.len());
    if patch.len() <= remaining {
        bytes.extend_from_slice(&patch);
    } else {
        bytes.extend_from_slice(&patch[..remaining]);
        *truncated = true;
    }
    Ok(())
}

fn patch_with_modes(
    path: &Path,
    patch: &[u8],
    old_mode: Option<u32>,
    new_mode: Option<u32>,
) -> Result<Vec<u8>, LocalGitFailure> {
    let mode = match (old_mode, new_mode) {
        (Some(old_mode), Some(new_mode)) if old_mode != new_mode => {
            format!("old mode {old_mode:06o}\nnew mode {new_mode:06o}\n")
        }
        (None, Some(new_mode)) => format!("new file mode {new_mode:06o}\n"),
        (Some(old_mode), None) => format!("deleted file mode {old_mode:06o}\n"),
        _ => return Ok(patch.to_vec()),
    };
    if patch.is_empty() {
        let old_path = quoted_diff_path(b"a/", path);
        let new_path = quoted_diff_path(b"b/", path);
        let mut rendered = Vec::new();
        rendered.extend_from_slice(b"diff --git ");
        rendered.extend_from_slice(&old_path);
        rendered.push(b' ');
        rendered.extend_from_slice(&new_path);
        rendered.push(b'\n');
        rendered.extend_from_slice(mode.as_bytes());
        return Ok(rendered);
    }
    let first_line = patch
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|position| position + 1)
        .ok_or(LocalGitFailure::Operation)?;
    let existing_mode_end = patch[first_line..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|position| first_line + position + 1)
        .filter(|end| {
            patch[first_line..*end].starts_with(b"new file mode ")
                || patch[first_line..*end].starts_with(b"deleted file mode ")
        })
        .unwrap_or(first_line);
    let mut rendered = Vec::with_capacity(patch.len().saturating_add(mode.len()));
    rendered.extend_from_slice(&patch[..first_line]);
    rendered.extend_from_slice(mode.as_bytes());
    rendered.extend_from_slice(&patch[existing_mode_end..]);
    Ok(rendered)
}

fn quoted_diff_path(prefix: &[u8], path: &Path) -> Vec<u8> {
    let path = [prefix, path.as_os_str().as_bytes()].concat();
    if path
        .iter()
        .all(|byte| matches!(byte, b'!'..=b'~') && !matches!(byte, b'"' | b'\\'))
    {
        return path;
    }
    let mut quoted = Vec::with_capacity(path.len().saturating_add(2));
    quoted.push(b'"');
    for byte in path {
        match byte {
            b'\\' => quoted.extend_from_slice(b"\\\\"),
            b'"' => quoted.extend_from_slice(b"\\\""),
            b'\x07' => quoted.extend_from_slice(b"\\a"),
            b'\x08' => quoted.extend_from_slice(b"\\b"),
            b'\t' => quoted.extend_from_slice(b"\\t"),
            b'\n' => quoted.extend_from_slice(b"\\n"),
            b'\x0b' => quoted.extend_from_slice(b"\\v"),
            b'\x0c' => quoted.extend_from_slice(b"\\f"),
            b'\r' => quoted.extend_from_slice(b"\\r"),
            b' '..=b'~' => quoted.push(byte),
            _ => {
                quoted.push(b'\\');
                quoted.push(b'0' + ((byte >> 6) & 0x07));
                quoted.push(b'0' + ((byte >> 3) & 0x07));
                quoted.push(b'0' + (byte & 0x07));
            }
        }
    }
    quoted.push(b'"');
    quoted
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

fn log(
    repository: &Repository,
    authority: &PinnedRepository,
    arguments: GitLogArguments,
) -> Result<LogResult, LocalGitFailure> {
    let start = resolve_bounded_commit(repository, authority, &arguments.revision)?.id();
    let shallow = read_shallow_boundaries(authority)?;
    let (ordered, truncated) =
        bounded_topological_page(repository, start, arguments.max_entries, &shallow)?;
    let mut commits = Vec::new();
    for oid in ordered {
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

fn read_shallow_boundaries(
    authority: &PinnedRepository,
) -> Result<HashSet<git2::Oid>, LocalGitFailure> {
    let descriptor = match openat(
        &authority.git_directory,
        "shallow",
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(HashSet::new()),
        Err(_) => return Err(LocalGitFailure::Repository),
    };
    let mut file = fs::File::from(descriptor);
    let metadata = file.metadata().map_err(|_| LocalGitFailure::Repository)?;
    if !metadata.is_file() || metadata.len() > MAX_SHALLOW_BYTES as u64 {
        return Err(LocalGitFailure::Repository);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take((MAX_SHALLOW_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalGitFailure::Repository)?;
    let mut boundaries = HashSet::new();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        if boundaries.len() == MAX_SHALLOW_ENTRIES {
            return Err(LocalGitFailure::Repository);
        }
        let value = std::str::from_utf8(line).map_err(|_| LocalGitFailure::Repository)?;
        boundaries.insert(git2::Oid::from_str(value).map_err(|_| LocalGitFailure::Repository)?);
    }
    Ok(boundaries)
}

fn bounded_topological_page(
    repository: &Repository,
    start: git2::Oid,
    limit: usize,
    shallow: &HashSet<git2::Oid>,
) -> Result<(Vec<git2::Oid>, bool), LocalGitFailure> {
    let mut frontier = vec![start];
    let mut queued = HashSet::from([start]);
    let mut emitted = HashSet::new();
    let mut ordered = Vec::with_capacity(limit);
    let mut topology_inspections = 0_usize;
    while !frontier.is_empty() && ordered.len() < limit {
        let selected = select_topological_candidate(
            repository,
            &frontier,
            shallow,
            &mut topology_inspections,
        )?;
        let oid = frontier.remove(selected);
        queued.remove(&oid);
        if !emitted.insert(oid) {
            continue;
        }
        let commit = find_bounded_commit(repository, oid)?;
        let parents = if shallow.contains(&oid) {
            Vec::new()
        } else {
            commit.parent_ids().collect::<Vec<_>>()
        };
        ordered.push(oid);
        for parent in parents {
            if validate_object_header(repository, parent)? != git2::ObjectType::Commit {
                return Err(LocalGitFailure::Operation);
            }
            if !emitted.contains(&parent) && queued.insert(parent) {
                frontier.push(parent);
            }
        }
    }
    let truncated = !frontier.is_empty();
    Ok((ordered, truncated))
}

fn select_topological_candidate(
    repository: &Repository,
    frontier: &[git2::Oid],
    shallow: &HashSet<git2::Oid>,
    inspections: &mut usize,
) -> Result<usize, LocalGitFailure> {
    for candidate_index in 0..frontier.len() {
        let candidate = frontier[candidate_index];
        let mut is_ancestor = false;
        for (other_index, other) in frontier.iter().copied().enumerate() {
            if candidate_index != other_index
                && bounded_commit_reaches(repository, other, candidate, shallow, inspections)?
            {
                is_ancestor = true;
                break;
            }
        }
        if !is_ancestor {
            return Ok(candidate_index);
        }
    }
    Err(LocalGitFailure::Operation)
}

fn bounded_commit_reaches(
    repository: &Repository,
    descendant: git2::Oid,
    ancestor: git2::Oid,
    shallow: &HashSet<git2::Oid>,
    inspections: &mut usize,
) -> Result<bool, LocalGitFailure> {
    let mut pending = vec![descendant];
    let mut visited = HashSet::new();
    while let Some(oid) = pending.pop() {
        if oid == ancestor {
            return Ok(true);
        }
        if !visited.insert(oid) {
            continue;
        }
        *inspections = inspections
            .checked_add(1)
            .filter(|count| *count <= MAX_WORKTREE_INSPECTIONS)
            .ok_or(LocalGitFailure::Operation)?;
        let commit = find_bounded_commit(repository, oid)?;
        if !shallow.contains(&oid) {
            pending.extend(commit.parent_ids());
        }
    }
    Ok(false)
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
    validate_tree_discovery_with_symlinks(repository, root, true)
}

fn validate_checkout_tree_discovery(
    repository: &Repository,
    root: &git2::Tree<'_>,
) -> Result<(), LocalGitFailure> {
    validate_tree_discovery_with_symlinks(repository, root, false)
}

fn validate_tree_discovery_with_symlinks(
    repository: &Repository,
    root: &git2::Tree<'_>,
    allow_symlinks: bool,
) -> Result<(), LocalGitFailure> {
    let object_database = repository.odb().map_err(|_| LocalGitFailure::Operation)?;
    let mut pending = vec![(root.id(), PathBuf::new())];
    let mut inspected = 0_usize;
    let mut inspected_path_bytes = 0_usize;
    let mut inspected_blob_bytes = 0_usize;
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
                    if !matches!(entry.filemode(), 0o100644 | 0o100755)
                        && !(allow_symlinks && entry.filemode() == 0o120000)
                    {
                        return Err(LocalGitFailure::Operation);
                    }
                    let (size, kind) = object_database
                        .read_header(entry.id())
                        .map_err(|_| LocalGitFailure::Operation)?;
                    inspected_blob_bytes = inspected_blob_bytes.saturating_add(size);
                    if kind != git2::ObjectType::Blob
                        || size > MAX_OBJECT_BYTES
                        || inspected_blob_bytes > MAX_TREE_BLOB_BYTES
                    {
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

fn validate_index_entry_count(index: &Index) -> Result<(), LocalGitFailure> {
    if index.len() > MAX_INDEX_ENTRIES {
        return Err(LocalGitFailure::Operation);
    }
    Ok(())
}

fn validate_index_objects(repository: &Repository, index: &Index) -> Result<(), LocalGitFailure> {
    validate_index_entry_count(index)?;
    let object_database = repository.odb().map_err(|_| LocalGitFailure::Operation)?;
    let mut blob_bytes = 0_usize;
    for entry in index.iter().filter(|entry| entry.flags & 0x3000 == 0) {
        if entry.mode == GITLINK_MODE {
            continue;
        }
        let (size, kind) = object_database
            .read_header(entry.id)
            .map_err(|_| LocalGitFailure::Operation)?;
        blob_bytes = blob_bytes.saturating_add(size);
        if kind != git2::ObjectType::Blob
            || size > MAX_OBJECT_BYTES
            || blob_bytes > MAX_TREE_BLOB_BYTES
        {
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
    authority: &PinnedRepository,
    revision: &str,
) -> Result<git2::Commit<'repository>, LocalGitFailure> {
    let mut oid = resolve_exact_revision_oid(authority, revision)?;
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
    authority: &PinnedRepository,
    revision: &str,
) -> Result<git2::Tree<'repository>, LocalGitFailure> {
    let mut oid = resolve_exact_revision_oid(authority, revision)?;
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

fn resolve_exact_revision_oid(
    authority: &PinnedRepository,
    revision: &str,
) -> Result<git2::Oid, LocalGitFailure> {
    if let Ok(oid) = git2::Oid::from_str(revision) {
        return Ok(oid);
    }
    let (_, target) = resolve_pinned_reference_chain_from(authority, revision, None)?;
    target.ok_or(LocalGitFailure::Operation)
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

fn commit<ValidateRoot>(
    repository: &mut Repository,
    identity: &GitIdentity,
    arguments: GitCommitArguments,
    authority: &PinnedRepository,
    persistent_object_database: &Odb<'_>,
    object_database: &Odb<'_>,
    validate_root_before_publish: ValidateRoot,
) -> Result<CommitResult, LocalGitFailure>
where
    ValidateRoot: FnOnce() -> Result<(), LocalGitFailure>,
{
    let (_index_lock, mut index) = IndexLock::acquire_for_repository(authority)?;
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
    let (reference_chain, initial_parent) = resolve_pinned_reference_chain(authority, None)?;
    let update_reference = reference_chain.last().ok_or(LocalGitFailure::Operation)?;
    if packed_reference_namespace_conflicts(authority, update_reference)? {
        return Err(LocalGitFailure::Operation);
    }
    let mut reference_locks = reference_chain
        .iter()
        .map(|reference| ReferenceLock::acquire(authority, reference))
        .collect::<Result<Vec<_>, _>>()?;
    let (locked_chain, parent) = resolve_pinned_reference_chain(authority, Some(&reference_locks))?;
    if locked_chain != reference_chain || parent != initial_parent {
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
    persist_objects(
        authority,
        repository,
        persistent_object_database,
        object_database,
        &[PackRoot::Commit(oid)],
    )?;
    let update_reference = locked_chain.last().ok_or(LocalGitFailure::Operation)?;
    if reference_locks
        .iter()
        .any(|lock| !lock.hierarchy_is_current(authority))
    {
        return Err(LocalGitFailure::Operation);
    }
    let (current_chain, current_parent) =
        resolve_pinned_reference_chain(authority, Some(&reference_locks))?;
    if current_chain != locked_chain || current_parent != parent {
        return Err(LocalGitFailure::Operation);
    }
    if packed_reference_namespace_conflicts(authority, update_reference)? {
        return Err(LocalGitFailure::Operation);
    }
    let update_lock = reference_locks
        .iter()
        .position(|lock| lock.name == *update_reference)
        .map(|position| reference_locks.swap_remove(position))
        .ok_or(LocalGitFailure::Operation)?;
    let old = parent.unwrap_or(git2::Oid::ZERO_SHA1);
    validate_root_before_publish()?;
    publish_commit_reference(
        authority,
        update_lock,
        update_reference,
        old,
        oid,
        &signature,
    )?;
    let state_cleaned = state != RepositoryState::Merge || repository.cleanup_state().is_ok();
    Ok(CommitResult {
        commit: oid.to_string(),
        state_cleaned,
    })
}

struct ReferenceLogLock {
    parent: OwnedFd,
    leaf: OsString,
    lock_name: OsString,
    lock: fs::File,
    identity: FileIdentity,
    backup: Option<fs::File>,
    original_permissions: Option<fs::Permissions>,
    committed: bool,
}

impl ReferenceLogLock {
    fn acquire(authority: &PinnedRepository, reference: &str) -> Result<Self, LocalGitFailure> {
        let git_directory =
            dup(&authority.git_directory).map_err(|_| LocalGitFailure::Operation)?;
        let refs = openat(
            &git_directory,
            "refs",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        let (directory_mode, file_mode) = reference_installation_modes(&refs)?;
        let logs = open_or_create_ref_directory_with_mode(
            &git_directory,
            OsStr::new("logs"),
            directory_mode,
        )?;
        let path = Path::new(reference);
        let leaf = path
            .file_name()
            .filter(|leaf| !leaf.is_empty())
            .ok_or(LocalGitFailure::Operation)?
            .to_owned();
        let mut parent = logs;
        if let Some(components) = path.parent() {
            for component in components.components() {
                let Component::Normal(component) = component else {
                    return Err(LocalGitFailure::Operation);
                };
                parent =
                    open_or_create_ref_directory_with_mode(&parent, component, directory_mode)?;
            }
        }
        let mut lock_name = OsString::from(&leaf);
        lock_name.push(".lock");
        let descriptor = openat(
            &parent,
            &lock_name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            file_mode,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        let lock = fs::File::from(descriptor);
        let identity = file_identity(&lock.metadata().map_err(|_| LocalGitFailure::Operation)?);
        let mut guard = Self {
            parent,
            leaf,
            lock_name,
            lock,
            identity,
            backup: None,
            original_permissions: None,
            committed: false,
        };
        guard
            .lock
            .set_permissions(fs::Permissions::from_mode(file_mode.bits()))
            .map_err(|_| LocalGitFailure::Operation)?;
        guard.copy_existing()?;
        Ok(guard)
    }

    fn copy_existing(&mut self) -> Result<(), LocalGitFailure> {
        let descriptor = match openat(
            &self.parent,
            &self.leaf,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(descriptor) => descriptor,
            Err(error) if error == rustix::io::Errno::NOENT => return Ok(()),
            Err(_) => return Err(LocalGitFailure::Operation),
        };
        let mut source = fs::File::from(descriptor);
        let metadata = source.metadata().map_err(|_| LocalGitFailure::Operation)?;
        if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > MAX_REFLOG_BYTES as u64
        {
            return Err(LocalGitFailure::Operation);
        }
        let permissions = fs::Permissions::from_mode(metadata.mode() & 0o777);
        self.lock
            .set_permissions(permissions.clone())
            .map_err(|_| LocalGitFailure::Operation)?;
        let copied = std::io::copy(
            &mut Read::by_ref(&mut source).take((MAX_REFLOG_BYTES + 1) as u64),
            &mut self.lock,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        if copied != metadata.len() || copied > MAX_REFLOG_BYTES as u64 {
            return Err(LocalGitFailure::Operation);
        }
        source.rewind().map_err(|_| LocalGitFailure::Operation)?;
        let mut backup = tempfile::tempfile().map_err(|_| LocalGitFailure::Operation)?;
        let backup_bytes = std::io::copy(
            &mut Read::by_ref(&mut source).take((MAX_REFLOG_BYTES + 1) as u64),
            &mut backup,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        if backup_bytes != metadata.len() || backup_bytes > MAX_REFLOG_BYTES as u64 {
            return Err(LocalGitFailure::Operation);
        }
        backup.rewind().map_err(|_| LocalGitFailure::Operation)?;
        self.backup = Some(backup);
        self.original_permissions = Some(permissions);
        Ok(())
    }

    fn append(
        &mut self,
        old: git2::Oid,
        new: git2::Oid,
        signature: &Signature<'_>,
        action: &str,
    ) -> Result<(), LocalGitFailure> {
        let time = signature.when();
        let offset = time.offset_minutes();
        let sign = if offset < 0 { '-' } else { '+' };
        let absolute_offset = offset.unsigned_abs();
        writeln!(
            self.lock,
            "{old} {new} {} <{}> {} {sign}{:02}{:02}\t{action}",
            signature.name().map_err(|_| LocalGitFailure::Operation)?,
            signature.email().map_err(|_| LocalGitFailure::Operation)?,
            time.seconds(),
            absolute_offset / 60,
            absolute_offset % 60,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        if self
            .lock
            .metadata()
            .map_err(|_| LocalGitFailure::Operation)?
            .len()
            > MAX_REFLOG_BYTES as u64
        {
            return Err(LocalGitFailure::Operation);
        }
        self.lock.sync_all().map_err(|_| LocalGitFailure::Operation)
    }

    fn publish(&mut self) -> Result<(), LocalGitFailure> {
        if !self.path_still_owned() {
            return Err(LocalGitFailure::Operation);
        }
        renameat_with(
            &self.parent,
            &self.lock_name,
            &self.parent,
            &self.leaf,
            RenameFlags::empty(),
        )
        .map_err(|_| LocalGitFailure::Operation)?;
        self.committed = true;
        Ok(())
    }

    fn rollback(&mut self) -> Result<(), LocalGitFailure> {
        if !self.committed {
            return Ok(());
        }
        if let Some(backup) = &mut self.backup {
            let descriptor = openat(
                &self.parent,
                &self.lock_name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(|_| LocalGitFailure::Operation)?;
            let mut restoration = fs::File::from(descriptor);
            let identity = file_identity(
                &restoration
                    .metadata()
                    .map_err(|_| LocalGitFailure::Operation)?,
            );
            if let Some(permissions) = &self.original_permissions
                && restoration.set_permissions(permissions.clone()).is_err()
            {
                remove_owned_pack_lock(&self.parent, &self.lock_name, &restoration, identity);
                return Err(LocalGitFailure::Operation);
            }
            if backup.rewind().is_err() {
                remove_owned_pack_lock(&self.parent, &self.lock_name, &restoration, identity);
                return Err(LocalGitFailure::Operation);
            }
            let copied = match std::io::copy(
                &mut Read::by_ref(backup).take((MAX_REFLOG_BYTES + 1) as u64),
                &mut restoration,
            ) {
                Ok(copied) => copied,
                Err(_) => {
                    remove_owned_pack_lock(&self.parent, &self.lock_name, &restoration, identity);
                    return Err(LocalGitFailure::Operation);
                }
            };
            if copied > MAX_REFLOG_BYTES as u64 {
                remove_owned_pack_lock(&self.parent, &self.lock_name, &restoration, identity);
                return Err(LocalGitFailure::Operation);
            }
            if restoration.sync_all().is_err() {
                remove_owned_pack_lock(&self.parent, &self.lock_name, &restoration, identity);
                return Err(LocalGitFailure::Operation);
            }
            if !pack_lock_is_owned(&self.parent, &self.lock_name, &restoration, identity) {
                return Err(LocalGitFailure::Operation);
            }
            let published_identity = openat(
                &self.parent,
                &self.leaf,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .ok()
            .and_then(|descriptor| fs::File::from(descriptor).metadata().ok())
            .map(|metadata| file_identity(&metadata));
            if published_identity != Some(self.identity) {
                remove_owned_pack_lock(&self.parent, &self.lock_name, &restoration, identity);
                return Err(LocalGitFailure::Operation);
            }
            if renameat_with(
                &self.parent,
                &self.lock_name,
                &self.parent,
                &self.leaf,
                RenameFlags::empty(),
            )
            .is_err()
            {
                remove_owned_pack_lock(&self.parent, &self.lock_name, &restoration, identity);
                return Err(LocalGitFailure::Operation);
            }
        } else {
            let published_identity = openat(
                &self.parent,
                &self.leaf,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .ok()
            .and_then(|descriptor| fs::File::from(descriptor).metadata().ok())
            .map(|metadata| file_identity(&metadata));
            if published_identity != Some(self.identity) {
                return Err(LocalGitFailure::Operation);
            }
            unlinkat(&self.parent, &self.leaf, AtFlags::empty())
                .map_err(|_| LocalGitFailure::Operation)?;
        }
        self.committed = false;
        Ok(())
    }

    fn path_still_owned(&self) -> bool {
        let descriptor_identity = self
            .lock
            .metadata()
            .map(|metadata| file_identity(&metadata))
            .ok();
        let path_identity = openat(
            &self.parent,
            &self.lock_name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()
        .and_then(|descriptor| fs::File::from(descriptor).metadata().ok())
        .map(|metadata| file_identity(&metadata));
        descriptor_identity == Some(self.identity) && path_identity == Some(self.identity)
    }
}

impl Drop for ReferenceLogLock {
    fn drop(&mut self) {
        if !self.committed && self.path_still_owned() {
            let _ = unlinkat(&self.parent, &self.lock_name, AtFlags::empty());
        }
    }
}

fn publish_commit_reference(
    authority: &PinnedRepository,
    update_lock: ReferenceLock,
    update_reference: &str,
    old: git2::Oid,
    new: git2::Oid,
    signature: &Signature<'_>,
) -> Result<(), LocalGitFailure> {
    publish_commit_reference_with_hook(
        authority,
        update_lock,
        update_reference,
        old,
        new,
        signature,
        || {},
    )
}

fn publish_commit_reference_with_hook<Hook: FnOnce()>(
    authority: &PinnedRepository,
    mut update_lock: ReferenceLock,
    update_reference: &str,
    old: git2::Oid,
    new: git2::Oid,
    signature: &Signature<'_>,
    before_reference_publish: Hook,
) -> Result<(), LocalGitFailure> {
    update_lock.prepare(authority, new)?;
    let mut logs = vec![ReferenceLogLock::acquire(authority, "HEAD")?];
    if update_reference != "HEAD" {
        logs.push(ReferenceLogLock::acquire(authority, update_reference)?);
    }
    for log in &mut logs {
        log.append(old, new, signature, "commit: fixer agent")?;
    }
    let mut published = 0_usize;
    while published < logs.len() {
        if logs[published].publish().is_err() {
            rollback_published_logs(&mut logs[..published]);
            return Err(LocalGitFailure::Operation);
        }
        published += 1;
    }
    before_reference_publish();
    if update_lock.publish(authority).is_err() {
        rollback_published_logs(&mut logs);
        return Err(LocalGitFailure::Operation);
    }
    Ok(())
}

fn publish_symbolic_head(
    authority: &PinnedRepository,
    mut head_lock: ReferenceLock,
    target_reference: &str,
    old: git2::Oid,
    new: git2::Oid,
    signature: &Signature<'_>,
) -> Result<(), LocalGitFailure> {
    head_lock.prepare_symbolic(authority, target_reference)?;
    let mut log = ReferenceLogLock::acquire(authority, "HEAD")?;
    log.append(
        old,
        new,
        signature,
        "checkout: moving to configured local branch",
    )?;
    log.publish()?;
    if head_lock.publish(authority).is_err() {
        let _ = log.rollback();
        return Err(LocalGitFailure::Operation);
    }
    Ok(())
}

fn rollback_published_logs(logs: &mut [ReferenceLogLock]) {
    for log in logs.iter_mut().rev() {
        let _ = log.rollback();
    }
}

#[derive(Clone, Copy)]
enum PackRoot {
    Object(git2::Oid),
    Commit(git2::Oid),
}

fn persist_objects(
    authority: &PinnedRepository,
    repository: &Repository,
    persistent_objects: &Odb<'_>,
    object_database: &Odb<'_>,
    roots: &[PackRoot],
) -> Result<(), LocalGitFailure> {
    if roots.is_empty() {
        return Ok(());
    }
    let mut buffer = Buf::new();
    let mut builder = repository
        .packbuilder()
        .map_err(|_| LocalGitFailure::Operation)?;
    let mut inserted = HashSet::new();
    for root in roots {
        match root {
            PackRoot::Object(oid) => {
                insert_missing_pack_object(persistent_objects, &mut builder, &mut inserted, *oid)?;
            }
            PackRoot::Commit(oid) => {
                insert_missing_pack_object(persistent_objects, &mut builder, &mut inserted, *oid)?;
                let tree = repository
                    .find_commit(*oid)
                    .map_err(|_| LocalGitFailure::Operation)?
                    .tree_id();
                insert_missing_tree_graph(
                    repository,
                    persistent_objects,
                    &mut builder,
                    &mut inserted,
                    tree,
                )?;
            }
        }
    }
    if inserted.is_empty() {
        return Ok(());
    }
    builder
        .write_buf(&mut buffer)
        .map_err(|_| LocalGitFailure::Operation)?;
    let indexed = tempfile::tempdir().map_err(|_| LocalGitFailure::Operation)?;
    let mut indexer = Indexer::new(Some(object_database), indexed.path(), 0o600, true)
        .map_err(|_| LocalGitFailure::Operation)?;
    indexer
        .write_all(&buffer)
        .map_err(|_| LocalGitFailure::Operation)?;
    let checksum = indexer.commit().map_err(|_| LocalGitFailure::Operation)?;
    let objects = openat(
        &authority.git_directory,
        "objects",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitFailure::Repository)?;
    let pack = openat(
        &objects,
        "pack",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitFailure::Repository)?;
    let installation_mode = pack_installation_mode(&pack)?;
    let stem = format!("pack-{checksum}");
    install_packed_object_file(
        &pack,
        &indexed.path().join(format!("{stem}.pack")),
        installation_mode,
    )?;
    install_packed_object_file(
        &pack,
        &indexed.path().join(format!("{stem}.idx")),
        installation_mode,
    )
}

fn insert_missing_pack_object(
    persistent_objects: &Odb<'_>,
    builder: &mut PackBuilder<'_>,
    inserted: &mut HashSet<git2::Oid>,
    oid: git2::Oid,
) -> Result<(), LocalGitFailure> {
    if !persistent_objects.exists(oid) && inserted.insert(oid) {
        builder
            .insert_object(oid, None)
            .map_err(|_| LocalGitFailure::Operation)?;
    }
    Ok(())
}

fn insert_missing_tree_graph(
    repository: &Repository,
    persistent_objects: &Odb<'_>,
    builder: &mut PackBuilder<'_>,
    inserted: &mut HashSet<git2::Oid>,
    root: git2::Oid,
) -> Result<(), LocalGitFailure> {
    let mut pending = vec![root];
    while let Some(oid) = pending.pop() {
        if persistent_objects.exists(oid) || !inserted.insert(oid) {
            continue;
        }
        builder
            .insert_object(oid, None)
            .map_err(|_| LocalGitFailure::Operation)?;
        let tree = repository
            .find_tree(oid)
            .map_err(|_| LocalGitFailure::Operation)?;
        for entry in &tree {
            match entry.kind() {
                Some(git2::ObjectType::Tree) => pending.push(entry.id()),
                Some(git2::ObjectType::Blob) => {
                    insert_missing_pack_object(persistent_objects, builder, inserted, entry.id())?
                }
                Some(git2::ObjectType::Commit) if entry.filemode() == GITLINK_MODE as i32 => {}
                _ => return Err(LocalGitFailure::Operation),
            }
        }
    }
    Ok(())
}

fn pack_installation_mode(pack_directory: &OwnedFd) -> Result<Mode, LocalGitFailure> {
    let metadata = fs::File::from(dup(pack_directory).map_err(|_| LocalGitFailure::Operation)?)
        .metadata()
        .map_err(|_| LocalGitFailure::Operation)?;
    let mode = (metadata.mode() & 0o666) | 0o600;
    Ok(Mode::from_raw_mode(mode))
}

fn install_packed_object_file(
    pack_directory: &OwnedFd,
    source_path: &Path,
    mode: Mode,
) -> Result<(), LocalGitFailure> {
    install_packed_object_file_with_hook(pack_directory, source_path, mode, || {})
}

fn install_packed_object_file_with_hook<Hook: FnOnce()>(
    pack_directory: &OwnedFd,
    source_path: &Path,
    mode: Mode,
    before_publish: Hook,
) -> Result<(), LocalGitFailure> {
    install_packed_object_file_with_copy_and_hook(
        pack_directory,
        source_path,
        mode,
        std::io::copy,
        before_publish,
    )
}

fn install_packed_object_file_with_copy_and_hook<Copy, Hook>(
    pack_directory: &OwnedFd,
    source_path: &Path,
    mode: Mode,
    copy: Copy,
    before_publish: Hook,
) -> Result<(), LocalGitFailure>
where
    Copy: FnOnce(&mut fs::File, &mut fs::File) -> std::io::Result<u64>,
    Hook: FnOnce(),
{
    let name = source_path.file_name().ok_or(LocalGitFailure::Operation)?;
    let mut temporary_name = OsString::from(name);
    temporary_name.push(".lock");
    let mut source = fs::File::open(source_path).map_err(|_| LocalGitFailure::Operation)?;
    let source_length = source
        .metadata()
        .map_err(|_| LocalGitFailure::Operation)?
        .len();
    let descriptor = openat(
        pack_directory,
        &temporary_name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        mode,
    )
    .map_err(|_| LocalGitFailure::Operation)?;
    let mut destination = fs::File::from(descriptor);
    let identity = file_identity(
        &destination
            .metadata()
            .map_err(|_| LocalGitFailure::Operation)?,
    );
    if destination
        .set_permissions(fs::Permissions::from_mode(mode.bits()))
        .is_err()
    {
        remove_owned_pack_lock(pack_directory, &temporary_name, &destination, identity);
        return Err(LocalGitFailure::Operation);
    }
    let copied = match copy(&mut source, &mut destination) {
        Ok(copied) => copied,
        Err(_) => {
            remove_owned_pack_lock(pack_directory, &temporary_name, &destination, identity);
            return Err(LocalGitFailure::Operation);
        }
    };
    if copied != source_length {
        remove_owned_pack_lock(pack_directory, &temporary_name, &destination, identity);
        return Err(LocalGitFailure::Operation);
    }
    if destination.sync_all().is_err() {
        remove_owned_pack_lock(pack_directory, &temporary_name, &destination, identity);
        return Err(LocalGitFailure::Operation);
    }
    before_publish();
    if !pack_lock_is_owned(pack_directory, &temporary_name, &destination, identity) {
        return Err(LocalGitFailure::Operation);
    }
    match renameat_with(
        pack_directory,
        &temporary_name,
        pack_directory,
        name,
        RenameFlags::NOREPLACE,
    ) {
        Ok(()) => Ok(()),
        Err(error) if error == rustix::io::Errno::EXIST => {
            remove_owned_pack_lock(pack_directory, &temporary_name, &destination, identity);
            let existing = openat(
                pack_directory,
                name,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| LocalGitFailure::Operation)?;
            let mut existing = fs::File::from(existing);
            let metadata = existing
                .metadata()
                .map_err(|_| LocalGitFailure::Operation)?;
            if metadata.is_file()
                && metadata.len() == source_length
                && files_have_equal_content(&mut source, &mut existing)?
            {
                Ok(())
            } else {
                Err(LocalGitFailure::Operation)
            }
        }
        Err(_) => {
            remove_owned_pack_lock(pack_directory, &temporary_name, &destination, identity);
            Err(LocalGitFailure::Operation)
        }
    }
}

fn pack_lock_is_owned(
    pack_directory: &OwnedFd,
    temporary_name: &OsStr,
    destination: &fs::File,
    identity: FileIdentity,
) -> bool {
    let descriptor_identity = destination
        .metadata()
        .ok()
        .map(|metadata| file_identity(&metadata));
    let path_identity = openat(
        pack_directory,
        temporary_name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()
    .and_then(|descriptor| fs::File::from(descriptor).metadata().ok())
    .map(|metadata| file_identity(&metadata));
    descriptor_identity == Some(identity) && path_identity == Some(identity)
}

fn remove_owned_pack_lock(
    pack_directory: &OwnedFd,
    temporary_name: &OsStr,
    destination: &fs::File,
    identity: FileIdentity,
) {
    if pack_lock_is_owned(pack_directory, temporary_name, destination, identity) {
        let _ = unlinkat(pack_directory, temporary_name, AtFlags::empty());
    }
}

fn files_have_equal_content(
    first: &mut fs::File,
    second: &mut fs::File,
) -> Result<bool, LocalGitFailure> {
    first
        .seek(std::io::SeekFrom::Start(0))
        .map_err(|_| LocalGitFailure::Operation)?;
    second
        .seek(std::io::SeekFrom::Start(0))
        .map_err(|_| LocalGitFailure::Operation)?;
    let mut first_buffer = [0_u8; 8192];
    let mut second_buffer = [0_u8; 8192];
    loop {
        let first_read = first
            .read(&mut first_buffer)
            .map_err(|_| LocalGitFailure::Operation)?;
        let second_read = second
            .read(&mut second_buffer)
            .map_err(|_| LocalGitFailure::Operation)?;
        if first_read != second_read || first_buffer[..first_read] != second_buffer[..second_read] {
            return Ok(false);
        }
        if first_read == 0 {
            return Ok(true);
        }
    }
}

fn branch_create<ValidateRoot>(
    repository: &Repository,
    authority: &PinnedRepository,
    arguments: GitBranchCreateArguments,
    validate_root_before_publish: ValidateRoot,
) -> Result<BranchResult, LocalGitFailure>
where
    ValidateRoot: FnOnce() -> Result<(), LocalGitFailure>,
{
    let commit = resolve_bounded_commit(repository, authority, &arguments.start)?;
    let head = commit.id().to_string();
    let reference_name = format!("refs/heads/{}", arguments.name);
    if packed_reference_exists(authority, &reference_name)? {
        return Err(LocalGitFailure::Operation);
    }
    create_loose_branch_reference(
        authority,
        &arguments.name,
        commit.id(),
        validate_root_before_publish,
    )?;
    Ok(BranchResult {
        branch: arguments.name,
        head,
    })
}

fn packed_reference_exists(
    authority: &PinnedRepository,
    reference_name: &str,
) -> Result<bool, LocalGitFailure> {
    for (_, existing) in read_packed_references(authority)? {
        let requested = reference_name.as_bytes();
        let requested_prefix = [requested, b"/"].concat();
        let existing_prefix = [existing.as_slice(), b"/"].concat();
        if existing == requested
            || existing.starts_with(&requested_prefix)
            || requested.starts_with(&existing_prefix)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn packed_reference_namespace_conflicts(
    authority: &PinnedRepository,
    reference_name: &str,
) -> Result<bool, LocalGitFailure> {
    for (_, existing) in read_packed_references(authority)? {
        let requested = reference_name.as_bytes();
        let requested_prefix = [requested, b"/"].concat();
        let existing_prefix = [existing.as_slice(), b"/"].concat();
        if existing != requested
            && (existing.starts_with(&requested_prefix) || requested.starts_with(&existing_prefix))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn packed_reference_target(
    authority: &PinnedRepository,
    reference_name: &str,
) -> Result<Option<git2::Oid>, LocalGitFailure> {
    Ok(read_packed_references(authority)?
        .into_iter()
        .find_map(|(oid, name)| (name == reference_name.as_bytes()).then_some(oid)))
}

fn read_packed_references(
    authority: &PinnedRepository,
) -> Result<Vec<(git2::Oid, Vec<u8>)>, LocalGitFailure> {
    let descriptor = match openat(
        &authority.git_directory,
        "packed-refs",
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(Vec::new()),
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
    let mut references = Vec::new();
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
        let existing = line
            .get(separator + 1..)
            .ok_or(LocalGitFailure::Operation)?;
        if existing.is_empty()
            || std::str::from_utf8(existing)
                .ok()
                .is_none_or(|name| !git2::Reference::is_valid_name(name))
        {
            return Err(LocalGitFailure::Operation);
        }
        references.push((oid, existing.to_vec()));
    }
    Ok(references)
}

fn create_loose_branch_reference<ValidateRoot>(
    authority: &PinnedRepository,
    branch: &str,
    target: git2::Oid,
    validate_root_before_publish: ValidateRoot,
) -> Result<(), LocalGitFailure>
where
    ValidateRoot: FnOnce() -> Result<(), LocalGitFailure>,
{
    create_loose_branch_reference_with_hooks(
        authority,
        branch,
        target,
        || {},
        validate_root_before_publish,
    )
}

#[cfg(test)]
fn create_loose_branch_reference_with_hook<Hook: FnOnce()>(
    authority: &PinnedRepository,
    branch: &str,
    target: git2::Oid,
    post_write: Hook,
) -> Result<(), LocalGitFailure> {
    create_loose_branch_reference_with_hooks(authority, branch, target, post_write, || Ok(()))
}

fn create_loose_branch_reference_with_hooks<Hook, ValidateRoot>(
    authority: &PinnedRepository,
    branch: &str,
    target: git2::Oid,
    post_write: Hook,
    validate_root_before_publish: ValidateRoot,
) -> Result<(), LocalGitFailure>
where
    Hook: FnOnce(),
    ValidateRoot: FnOnce() -> Result<(), LocalGitFailure>,
{
    let refs = openat(
        &authority.git_directory,
        "refs",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitFailure::Operation)?;
    let (directory_mode, file_mode) = reference_installation_modes(&refs)?;
    let mut directory =
        open_or_create_ref_directory_with_mode(&refs, OsStr::new("heads"), directory_mode)?;
    let mut components = Path::new(branch).components().peekable();
    let mut leaf = None;
    while let Some(component) = components.next() {
        let Component::Normal(component) = component else {
            return Err(LocalGitFailure::Operation);
        };
        if components.peek().is_some() {
            directory =
                open_or_create_ref_directory_with_mode(&directory, component, directory_mode)?;
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
        file_mode,
    )
    .map_err(|_| LocalGitFailure::Operation)?;
    let mut lock = fs::File::from(lock);
    let identity = file_identity(&lock.metadata().map_err(|_| LocalGitFailure::Operation)?);
    let lock_path = descriptor_path_from_fd(&directory).join(&lock_name);
    let outcome = (|| {
        lock.set_permissions(fs::Permissions::from_mode(file_mode.bits()))
            .map_err(|_| LocalGitFailure::Operation)?;
        writeln!(lock, "{target}").map_err(|_| LocalGitFailure::Operation)?;
        lock.sync_all().map_err(|_| LocalGitFailure::Operation)?;
        post_write();
        let reference_name = format!("refs/heads/{branch}");
        if packed_reference_exists(authority, &reference_name)? {
            return Err(LocalGitFailure::Operation);
        }
        let path_identity = fs::symlink_metadata(&lock_path)
            .map(|metadata| file_identity(&metadata))
            .map_err(|_| LocalGitFailure::Operation)?;
        let descriptor_identity =
            file_identity(&lock.metadata().map_err(|_| LocalGitFailure::Operation)?);
        if path_identity != identity || descriptor_identity != identity {
            return Err(LocalGitFailure::Operation);
        }
        validate_root_before_publish()?;
        renameat_with(
            &directory,
            &lock_name,
            &directory,
            &leaf,
            RenameFlags::NOREPLACE,
        )
        .map_err(|_| LocalGitFailure::Operation)
    })();
    let still_owned = fs::symlink_metadata(&lock_path)
        .map(|metadata| file_identity(&metadata) == identity)
        .unwrap_or(false);
    if outcome.is_err() && still_owned {
        let _ = unlinkat(&directory, &lock_name, AtFlags::empty());
    }
    outcome
}

fn reference_installation_modes(refs: &OwnedFd) -> Result<(Mode, Mode), LocalGitFailure> {
    let metadata = fs::File::from(dup(refs).map_err(|_| LocalGitFailure::Operation)?)
        .metadata()
        .map_err(|_| LocalGitFailure::Operation)?;
    let directory_mode = (metadata.mode() & 0o2777) | 0o700;
    let file_mode = (metadata.mode() & 0o666) | 0o600;
    Ok((
        Mode::from_raw_mode(directory_mode),
        Mode::from_raw_mode(file_mode),
    ))
}

fn open_or_create_ref_directory(
    parent: &OwnedFd,
    name: &OsStr,
) -> Result<OwnedFd, LocalGitFailure> {
    open_or_create_ref_directory_with_mode(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR)
}

fn open_or_create_ref_directory_with_mode(
    parent: &OwnedFd,
    name: &OsStr,
    mode: Mode,
) -> Result<OwnedFd, LocalGitFailure> {
    open_or_create_ref_directory_with_mode_tracked(parent, name, mode)
        .map(|(directory, _)| directory)
}

fn open_or_create_ref_directory_with_mode_tracked(
    parent: &OwnedFd,
    name: &OsStr,
    mode: Mode,
) -> Result<(OwnedFd, bool), LocalGitFailure> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match openat(parent, name, flags, Mode::empty()) {
        Ok(directory) => Ok((directory, false)),
        Err(error) if error == rustix::io::Errno::NOENT => {
            mkdirat(parent, name, mode).map_err(|_| LocalGitFailure::Operation)?;
            let directory = openat(parent, name, flags, Mode::empty())
                .map_err(|_| LocalGitFailure::Operation)?;
            fs::File::from(dup(&directory).map_err(|_| LocalGitFailure::Operation)?)
                .set_permissions(fs::Permissions::from_mode(mode.bits()))
                .map_err(|_| LocalGitFailure::Operation)?;
            Ok((directory, true))
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

    use git2::{
        BranchType, IndexAddOption, ObjectType, Oid, Repository, Signature, build::CheckoutBuilder,
    };
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
    const TARGET_CONTENT: &str = "target\n";
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

    struct ModeOnlyPathFixture {
        path: OsString,
        quoted_header: &'static str,
        unquoted_header: &'static str,
    }

    impl ModeOnlyPathFixture {
        fn non_utf8() -> Self {
            Self {
                path: OsString::from_vec(vec![b'n', 0xff, b'.', b't', b'x', b't']),
                quoted_header: "\"a/n\\377.txt\" \"b/n\\377.txt\"",
                unquoted_header: "",
            }
        }

        fn control() -> Self {
            Self {
                path: OsString::from("line\nbreak.txt"),
                quoted_header: "diff --git \"a/line\\nbreak.txt\" \"b/line\\nbreak.txt\"\n",
                unquoted_header: "diff --git a/line\nbreak.txt",
            }
        }

        fn path(&self) -> &Path {
            Path::new(&self.path)
        }

        fn quoted_header(&self) -> &str {
            self.quoted_header
        }

        fn unquoted_header(&self) -> &str {
            self.unquoted_header
        }
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

    fn index_extension(bytes: &[u8], signature: &[u8; 4]) -> Vec<u8> {
        let start = bytes
            .windows(signature.len())
            .position(|window| window == signature)
            .expect("fixture index extension exists");
        let size = u32::from_be_bytes(
            bytes[start + 4..start + 8]
                .try_into()
                .expect("fixture extension length exists"),
        ) as usize;
        bytes[start..start + 8 + size].to_vec()
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

    fn packed_object_counts(root: &Path) -> Vec<u32> {
        let mut counts = fs::read_dir(root.join(".git/objects/pack"))
            .expect("fixture pack directory reads")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "idx")
            })
            .map(|entry| {
                let bytes = fs::read(entry.path()).expect("fixture pack index reads");
                u32::from_be_bytes(
                    bytes[1028..1032]
                        .try_into()
                        .expect("fixture pack fanout count exists"),
                )
            })
            .collect::<Vec<_>>();
        counts.sort_unstable();
        counts
    }

    fn set_index_flags(repository: &Repository, path: &str, flags: u16) {
        let mut index = repository.index().expect("fixture index opens");
        let mut entry = clone_index_entry(
            &index
                .get_path(Path::new(path), 0)
                .expect("fixture index entry exists"),
        );
        entry.flags |= flags;
        index.add(&entry).expect("fixture flagged entry installs");
        index.write().expect("fixture flagged index writes");
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

    fn install_missing_skip_worktree_entry(fixture: &Fixture) {
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let mut index = repository.index().expect("fixture index opens");
        let mut entry = clone_index_entry(
            &index
                .get_path(Path::new(TRACKED_PATH), 0)
                .expect("fixture tracked entry exists"),
        );
        entry.flags_extended |= INDEX_SKIP_WORKTREE;
        index.add(&entry).expect("skip-worktree entry installs");
        index.write().expect("skip-worktree index writes");
        fs::remove_file(fixture.root().join(TRACKED_PATH))
            .expect("skip-worktree fixture file removes");
    }

    fn install_staged_missing_skip_worktree_entry(fixture: &Fixture) {
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let changed_blob = repository
            .blob(CHANGED_CONTENT.as_bytes())
            .expect("changed fixture blob writes");
        let mut index = repository.index().expect("fixture index opens");
        let mut entry = clone_index_entry(
            &index
                .get_path(Path::new(TRACKED_PATH), 0)
                .expect("fixture tracked entry exists"),
        );
        entry.id = changed_blob;
        entry.file_size = CHANGED_CONTENT.len() as u32;
        entry.flags_extended |= INDEX_SKIP_WORKTREE;
        index
            .add(&entry)
            .expect("staged skip-worktree entry installs");
        index.write().expect("staged skip-worktree index writes");
        fs::remove_file(fixture.root().join(TRACKED_PATH))
            .expect("staged skip-worktree fixture file removes");
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

    fn plant_sparse_pack(root: &Path, name: &str, bytes: u64) {
        let path = root.join(".git/objects/pack").join(name);
        fs::File::create(path)
            .expect("pack-budget fixture file creates")
            .set_len(bytes)
            .expect("pack-budget fixture length sets");
    }

    fn plant_shallow_entries(root: &Path, oid: Oid, count: usize) {
        fs::write(root.join(".git/shallow"), format!("{oid}\n").repeat(count))
            .expect("shallow-budget fixture writes");
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

    fn plant_linear_history(repository: &Repository, mut parent: Oid, count: usize) -> Oid {
        for sequence in 0..count {
            let tree = repository
                .find_commit(parent)
                .expect("linear-history parent exists")
                .tree_id();
            let raw = format!(
                "tree {tree}\nparent {parent}\nauthor Signalbox <fixer@example.test> {sequence} +0000\ncommitter Signalbox <fixer@example.test> {sequence} +0000\n\nlinear {sequence}\n"
            );
            parent = repository
                .odb()
                .expect("fixture object database opens")
                .write(ObjectType::Commit, raw.as_bytes())
                .expect("linear-history commit writes");
        }
        parent
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

    fn plant_index_over_blob_budget(repository: &Repository) {
        let mut index = repository.index().expect("fixture index opens");
        index.clear().expect("fixture index clears");
        let count = MAX_TREE_BLOB_BYTES / MAX_OBJECT_BYTES + 1;
        for sequence in 0..count {
            let mut bytes = vec![b'x'; MAX_OBJECT_BYTES];
            bytes[..std::mem::size_of::<usize>()].copy_from_slice(&sequence.to_le_bytes());
            let blob = repository.blob(&bytes).expect("fixture blob writes");
            let path = format!("aggregate-blob-{sequence:02}.txt");
            index
                .add(&IndexEntry {
                    ctime: IndexTime::new(0, 0),
                    mtime: IndexTime::new(0, 0),
                    dev: 0,
                    ino: 0,
                    mode: 0o100644,
                    uid: 0,
                    gid: 0,
                    file_size: MAX_OBJECT_BYTES as u32,
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

    fn aggregate_blob_tree_commit(repository: &Repository, parent: Oid) -> Oid {
        let bytes = vec![b'x'; MAX_OBJECT_BYTES];
        let blob = repository
            .blob(&bytes)
            .expect("aggregate-tree fixture blob writes");
        let mut builder = repository
            .treebuilder(None)
            .expect("aggregate-tree builder opens");
        let count = MAX_TREE_BLOB_BYTES / MAX_OBJECT_BYTES + 1;
        for sequence in 0..count {
            builder
                .insert(format!("large-{sequence:02}.bin"), blob, 0o100644)
                .expect("aggregate-tree entry inserts");
        }
        let tree = builder.write().expect("aggregate-tree writes");
        raw_commit_with_tree(repository, tree, parent)
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

    fn commit_rejects_reflog_without_wait(
        executor: LocalGitExecutor<LocalWorkspaceFileSystem>,
        fifo_path: PathBuf,
    ) -> bool {
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let rejected = executor
                .execute_operation(LocalOperation::Commit(GitCommitArguments {
                    message: MODEL_MESSAGE.to_owned(),
                }))
                .is_err();
            sender.send(rejected).expect("fixture result sends");
        });
        match receiver.recv_timeout(Duration::from_secs(2)) {
            Ok(rejected) => {
                worker.join().expect("fixture worker joins");
                rejected
            }
            Err(_) => {
                let unblock = openat(
                    CWD,
                    &fifo_path,
                    OFlags::WRONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                );
                worker.join().expect("fixture worker joins after unblock");
                drop(unblock);
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
                        &executor.repository_authority,
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
    fn status_treats_a_missing_skip_worktree_entry_as_unchanged() {
        let fixture = Fixture::new();
        install_missing_skip_worktree_entry(&fixture);
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);
        let entries = status["entries"]
            .as_array()
            .expect("status entries are an array");

        assert!(entries.is_empty());
    }

    #[test]
    fn status_treats_a_tracked_child_as_deleted_when_its_parent_becomes_a_file() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.root().join(NESTED_TRACKED_DIRECTORY))
            .expect("nested fixture directory constructs");
        fs::write(fixture.root().join(NESTED_TRACKED_PATH), INITIAL_CONTENT)
            .expect("nested tracked file writes");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        commit_all(&repository, INITIAL_MESSAGE);
        fs::remove_dir_all(fixture.root().join(NESTED_TRACKED_DIRECTORY))
            .expect("tracked parent directory removes");
        fs::write(
            fixture.root().join(NESTED_TRACKED_DIRECTORY),
            CHANGED_CONTENT,
        )
        .expect("replacement parent file writes");
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);

        assert_eq!(status["entries"][0]["path"], NESTED_TRACKED_DIRECTORY);
        assert_eq!(status["entries"][0]["worktree"], "untracked");
        assert_eq!(status["entries"][1]["path"], NESTED_TRACKED_PATH);
        assert_eq!(status["entries"][1]["worktree"], "deleted");
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
    fn index_lock_acquisition_failure_removes_the_created_lock() {
        let fixture = Fixture::new();
        let index_path = fixture.root().join(".git/index");
        let lock_path = fixture.root().join(".git/index.lock");
        let index = fs::OpenOptions::new()
            .write(true)
            .open(&index_path)
            .expect("fixture index opens");
        index
            .set_len((MAX_INDEX_BYTES + 1) as u64)
            .expect("oversized sparse index sets length");

        let failure = IndexLock::acquire(&index_path, &lock_path)
            .err()
            .expect("oversized index rejects lock acquisition");

        assert_eq!(failure, LocalGitFailure::Repository);
        assert!(!lock_path.exists());
    }

    #[test]
    fn index_lock_private_snapshot_failure_removes_the_created_lock() {
        let fixture = Fixture::new();
        let index_path = fixture.root().join(".git/index");
        let lock_path = fixture.root().join(".git/index.lock");

        let failure = IndexLock::acquire_with_private_directory(&index_path, &lock_path, || {
            Err(std::io::Error::other(
                "fixture private snapshot allocation fails",
            ))
        })
        .err()
        .expect("private snapshot allocation rejects lock acquisition");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert!(!lock_path.exists());
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
    fn read_only_worktree_tools_leave_no_repository_lock() {
        let fixture = Fixture::new();
        let executor = fixture.executor();

        execute(&executor, LocalOperation::Status);
        execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

        assert!(!fixture.root().join(".git/index.lock").exists());
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
    fn worktree_diff_treats_a_missing_skip_worktree_entry_as_unchanged() {
        let fixture = Fixture::new();
        install_missing_skip_worktree_entry(&fixture);
        let executor = fixture.executor();

        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

        assert_eq!(diff["patch"], "");
    }

    #[test]
    fn worktree_diff_includes_a_staged_missing_skip_worktree_entry() {
        let fixture = Fixture::new();
        install_staged_missing_skip_worktree_entry(&fixture);
        let executor = fixture.executor();

        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
        let patch = diff["patch"].as_str().expect("patch is text");

        assert!(patch.contains(&format!("-{}", INITIAL_CONTENT.trim_end())));
        assert!(patch.contains(&format!("+{}", CHANGED_CONTENT.trim_end())));
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
    fn worktree_diff_emits_the_executable_mode_for_an_untracked_file() {
        let fixture = Fixture::new();
        let path = fixture.root().join(UNTRACKED_PATH);
        fs::write(&path, UNTRACKED_CONTENT).expect("untracked fixture writes");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .expect("untracked executable mode sets");
        let expected_mode = 0o100000
            | (fs::metadata(&path)
                .expect("untracked fixture metadata reads")
                .permissions()
                .mode()
                & 0o777);
        let executor = fixture.executor();

        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
        let patch = diff["patch"].as_str().expect("patch is text");

        assert!(patch.contains(&format!("new file mode {expected_mode:06o}")));
    }

    #[test]
    fn worktree_diff_emits_the_executable_mode_for_a_deleted_file() {
        let fixture = Fixture::new();
        let path = fixture.root().join(TRACKED_PATH);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .expect("tracked executable mode sets");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        commit_all(&repository, "make tracked fixture executable");
        let expected_mode = repository
            .index()
            .expect("fixture index opens")
            .get_path(Path::new(TRACKED_PATH), 0)
            .expect("tracked fixture exists")
            .mode;
        fs::remove_file(&path).expect("tracked executable fixture removes");
        let executor = fixture.executor();

        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
        let patch = diff["patch"].as_str().expect("patch is text");

        assert!(patch.contains(&format!("deleted file mode {expected_mode:06o}")));
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
    fn worktree_diff_renders_a_tracked_directory_replaced_by_a_file() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let tracked_directory = "src";
        let tracked_child = "src/main.rs";
        let tracked_content = b"fn main() {}\n";
        let replacement_content = b"replacement file\n";
        fs::create_dir(fixture.root().join(tracked_directory))
            .expect("tracked fixture directory creates");
        fs::write(fixture.root().join(tracked_child), tracked_content)
            .expect("tracked fixture child writes");
        commit_all(&repository, "add tracked directory");
        fs::remove_dir_all(fixture.root().join(tracked_directory))
            .expect("tracked fixture directory removes");
        fs::write(fixture.root().join(tracked_directory), replacement_content)
            .expect("replacement fixture file writes");
        let executor = fixture.executor();

        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
        let patch = diff["patch"].as_str().expect("patch is text");

        assert!(patch.contains(&format!("--- a/{tracked_child}")));
        assert!(patch.contains(&format!(
            "-{}",
            String::from_utf8_lossy(tracked_content).trim_end()
        )));
        assert!(patch.contains(&format!("+++ b/{tracked_directory}")));
        assert!(patch.contains(&format!(
            "+{}",
            String::from_utf8_lossy(replacement_content).trim_end()
        )));
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
    fn worktree_diff_reports_a_non_utf8_mode_only_change() {
        let fixture = Fixture::new();
        let path = ModeOnlyPathFixture::non_utf8();
        fs::write(fixture.root().join(path.path()), INITIAL_CONTENT)
            .expect("non-UTF-8 fixture file writes");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let mut index = repository.index().expect("fixture index opens");
        index
            .add_path(path.path())
            .expect("non-UTF-8 fixture path stages");
        index.write().expect("fixture index writes");
        commit_index(&repository, INITIAL_MESSAGE);
        fs::set_permissions(
            fixture.root().join(path.path()),
            fs::Permissions::from_mode(0o755),
        )
        .expect("non-UTF-8 fixture executable mode sets");
        let executor = fixture.executor();

        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
        let patch = diff["patch"].as_str().expect("patch is text");

        assert_eq!(diff["truncated"], false);
        assert!(patch.contains(path.quoted_header()));
        assert!(patch.contains("old mode 100644"));
        assert!(patch.contains("new mode 100755"));
    }

    #[test]
    fn worktree_diff_quotes_control_bytes_in_a_mode_only_path() {
        let fixture = Fixture::new();
        let path = ModeOnlyPathFixture::control();
        fs::write(fixture.root().join(path.path()), INITIAL_CONTENT)
            .expect("control-path fixture file writes");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let mut index = repository.index().expect("fixture index opens");
        index
            .add_path(path.path())
            .expect("control-path fixture stages");
        index.write().expect("fixture index writes");
        commit_index(&repository, INITIAL_MESSAGE);
        fs::set_permissions(
            fixture.root().join(path.path()),
            fs::Permissions::from_mode(0o755),
        )
        .expect("control-path fixture executable mode sets");
        let executor = fixture.executor();

        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
        let patch = diff["patch"].as_str().expect("patch is text");

        assert!(patch.contains(path.quoted_header()));
        assert!(!patch.contains(path.unquoted_header()));
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
    fn revision_diff_rejects_a_fifo_reference_without_blocking() {
        let fixture = Fixture::new();
        let executor = fixture.executor();
        let reference_name = "refs/heads/fifo-revision";
        let reference_path = fixture.root().join(".git").join(reference_name);
        mkfifoat(CWD, &reference_path, Mode::RUSR | Mode::WUSR)
            .expect("revision fixture FIFO constructs");
        let repository = executor
            .repository_authority
            .repository()
            .expect("pinned fixture repository opens");

        let failure = diff(
            &repository,
            &executor.repository_authority,
            GitDiffArguments::Revisions {
                base: fixture.initial.to_string(),
                head: reference_name.to_owned(),
            },
            &executor.filesystem,
            &executor.root,
            Vec::new(),
        )
        .expect_err("FIFO revision rejects without libgit2 reopen");

        assert_eq!(failure, LocalGitFailure::Operation);
    }

    #[test]
    fn revision_diff_admits_an_unchanged_tracked_symlink() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let link_path = "tracked-link";
        symlink("target.txt", fixture.root().join(link_path))
            .expect("tracked fixture symlink creates");
        let with_symlink = commit_all(&repository, "add tracked symlink");
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("fixture ordinary change writes");
        let changed = commit_all(&repository, "change ordinary file");
        let executor = fixture.executor();

        let diff = execute(
            &executor,
            LocalOperation::Diff(GitDiffArguments::Revisions {
                base: with_symlink.to_string(),
                head: changed.to_string(),
            }),
        );
        let patch = diff["patch"].as_str().expect("patch is text");

        assert!(patch.contains(TRACKED_PATH));
        assert!(patch.contains(&format!("+{}", CHANGED_CONTENT.trim_end())));
    }

    #[test]
    fn revision_diff_reports_a_gitlink_change() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        install_gitlink(&repository, SUBMODULE_PATH, fixture.initial);
        let base = commit_index(&repository, "base gitlink");
        let changed_target = plant_linear_history(&repository, fixture.initial, 1);
        install_gitlink(&repository, SUBMODULE_PATH, changed_target);
        let head = commit_index(&repository, "changed gitlink");
        let executor = fixture.executor();

        let diff = execute(
            &executor,
            LocalOperation::Diff(GitDiffArguments::Revisions {
                base: base.to_string(),
                head: head.to_string(),
            }),
        );
        let patch = diff["patch"].as_str().expect("patch is text");

        assert!(patch.contains(SUBMODULE_PATH));
        assert!(patch.contains(&format!("-Subproject commit {}", fixture.initial)));
        assert!(patch.contains(&format!("+Subproject commit {changed_target}")));
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
    fn log_rejects_a_fifo_head_without_blocking() {
        let fixture = Fixture::new();
        let executor = fixture.executor();
        let head_path = fixture.root().join(".git/HEAD");
        fs::remove_file(&head_path).expect("fixture HEAD removes");
        mkfifoat(CWD, &head_path, Mode::RUSR | Mode::WUSR).expect("fixture HEAD FIFO constructs");
        let repository = executor
            .repository_authority
            .repository()
            .expect("pinned fixture repository opens");

        let failure = log(
            &repository,
            &executor.repository_authority,
            GitLogArguments {
                revision: "HEAD".to_owned(),
                max_entries: 1,
            },
        )
        .expect_err("FIFO HEAD rejects without libgit2 reopen");

        assert_eq!(failure, LocalGitFailure::Operation);
    }

    #[test]
    fn status_and_worktree_diff_reject_a_fifo_head_without_blocking() {
        let fixture = Fixture::new();
        let executor = fixture.executor();
        let head_path = fixture.root().join(".git/HEAD");
        fs::remove_file(&head_path).expect("fixture HEAD removes");
        mkfifoat(CWD, &head_path, Mode::RUSR | Mode::WUSR).expect("fixture HEAD FIFO constructs");
        let repository = executor
            .repository_authority
            .repository()
            .expect("pinned fixture repository opens");

        let status_failure = status(
            &repository,
            &executor.repository_authority,
            &executor.filesystem,
            &executor.root,
            Vec::new(),
        )
        .expect_err("status rejects a FIFO HEAD");
        let diff_failure = diff(
            &repository,
            &executor.repository_authority,
            GitDiffArguments::Worktree,
            &executor.filesystem,
            &executor.root,
            Vec::new(),
        )
        .expect_err("worktree diff rejects a FIFO HEAD");

        assert_eq!(status_failure, LocalGitFailure::Operation);
        assert_eq!(diff_failure, LocalGitFailure::Operation);
    }

    #[test]
    fn log_stops_after_the_requested_page_in_a_long_history() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let newest =
            plant_linear_history(&repository, fixture.initial, MAX_WORKTREE_INSPECTIONS + 1);
        let executor = fixture.executor();

        let log = execute(
            &executor,
            LocalOperation::Log(GitLogArguments {
                revision: newest.to_string(),
                max_entries: 1,
            }),
        );

        assert_eq!(log["commits"][0]["commit"], newest.to_string());
        assert_eq!(log["truncated"], true);
    }

    #[test]
    fn one_entry_log_does_not_order_an_unreturned_long_merge_parent() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let short_parent =
            commit_with_parents(&repository, &[fixture.initial], "short independent parent");
        let long_parent =
            plant_linear_history(&repository, fixture.initial, MAX_WORKTREE_INSPECTIONS + 1);
        let merge = commit_with_parents(
            &repository,
            &[short_parent, long_parent],
            "bounded merge page",
        );
        let executor = fixture.executor();

        let log = execute(
            &executor,
            LocalOperation::Log(GitLogArguments {
                revision: merge.to_string(),
                max_entries: 1,
            }),
        );

        assert_eq!(log["commits"][0]["commit"], merge.to_string());
        assert_eq!(log["truncated"], true);
    }

    #[test]
    fn log_honors_a_repository_shallow_boundary() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let boundary = plant_linear_history(&repository, fixture.initial, 1);
        let newest = plant_linear_history(&repository, boundary, 1);
        fs::write(fixture.root().join(".git/shallow"), format!("{boundary}\n"))
            .expect("fixture shallow boundary writes");
        let executor = fixture.executor();

        let log = execute(
            &executor,
            LocalOperation::Log(GitLogArguments {
                revision: newest.to_string(),
                max_entries: 10,
            }),
        );

        assert_eq!(
            log["commits"]
                .as_array()
                .expect("commits are an array")
                .len(),
            2
        );
        assert_eq!(log["commits"][0]["commit"], newest.to_string());
        assert_eq!(log["commits"][1]["commit"], boundary.to_string());
    }

    #[test]
    fn construction_rejects_a_shallow_file_over_the_entry_budget() {
        let fixture = Fixture::new();
        plant_shallow_entries(fixture.root(), fixture.initial, MAX_SHALLOW_ENTRIES + 1);

        let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
            .expect_err("over-budget shallow boundary rejects");

        assert!(matches!(error, LocalGitToolsConstructionError::Repository));
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
    fn log_rejects_an_exact_oid_before_loading_an_oversized_commit() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let oversized = oversized_commit_object(&repository, fixture.initial);
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Log(GitLogArguments {
                revision: oversized.to_string(),
                max_entries: 1,
            }))
            .expect_err("exact oversized commit object rejects");

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
    fn stage_preserves_repository_index_permissions() {
        let fixture = Fixture::new();
        fs::set_permissions(
            fixture.root().join(".git/index"),
            fs::Permissions::from_mode(0o660),
        )
        .expect("fixture index permissions set");
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("fixture change writes");
        let executor = fixture.executor();

        execute(
            &executor,
            LocalOperation::Stage(GitStageArguments {
                paths: vec![TRACKED_PATH.to_owned()],
            }),
        );
        let mode = fs::metadata(fixture.root().join(".git/index"))
            .expect("updated index metadata reads")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, 0o660);
    }

    #[test]
    fn stage_creates_a_missing_index_with_repository_shared_permissions() {
        let fixture = Fixture::new();
        let git_directory_mode = 0o2770;
        let expected_index_mode = (git_directory_mode & 0o666) | 0o600;
        fs::set_permissions(
            fixture.root().join(".git"),
            fs::Permissions::from_mode(git_directory_mode),
        )
        .expect("fixture Git directory permissions set");
        fs::remove_file(fixture.root().join(".git/index")).expect("fixture index removes");
        let executor = fixture.executor();

        execute(
            &executor,
            LocalOperation::Stage(GitStageArguments {
                paths: vec![TRACKED_PATH.to_owned()],
            }),
        );
        let installed_mode = fs::metadata(fixture.root().join(".git/index"))
            .expect("created index metadata reads")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(installed_mode, expected_index_mode);
    }

    #[test]
    fn stage_revalidates_the_injected_root_immediately_before_index_publication() {
        let parent = tempfile::tempdir().expect("workspace parent constructs");
        let root = parent.path().join("workspace");
        let retired = parent.path().join("retired");
        fs::create_dir(&root).expect("workspace root constructs");
        let original = Repository::init(&root).expect("original repository initializes");
        fs::write(root.join(TRACKED_PATH), INITIAL_CONTENT).expect("original fixture file writes");
        commit_all(&original, INITIAL_MESSAGE);
        fs::write(root.join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("original fixture change writes");
        let executor = LocalGitTools::try_new(LocalWorkspaceFileSystem, &root, identity())
            .expect("local Git suite constructs")
            .into_parts()
            .1;
        let original_index =
            fs::read(root.join(".git/index")).expect("original fixture index reads");
        let repository = executor
            .repository_authority
            .repository()
            .expect("pinned original repository opens");
        let mut replacement_index = Vec::new();

        let failure = executor
            .stage_with_pre_publish_hook(
                &repository,
                GitStageArguments {
                    paths: vec![TRACKED_PATH.to_owned()],
                },
                || {
                    fs::rename(&root, &retired).expect("original workspace retires");
                    fs::create_dir(&root).expect("replacement workspace constructs");
                    let replacement =
                        Repository::init(&root).expect("replacement repository initializes");
                    fs::write(root.join(TRACKED_PATH), INITIAL_CONTENT)
                        .expect("replacement fixture file writes");
                    commit_all(&replacement, INITIAL_MESSAGE);
                    replacement_index =
                        fs::read(root.join(".git/index")).expect("replacement fixture index reads");
                },
            )
            .expect_err("root replacement rejects before index publication");

        assert_eq!(failure, LocalGitFailure::Repository);
        assert_eq!(
            fs::read(retired.join(".git/index")).expect("retired index reads"),
            original_index
        );
        assert_eq!(
            fs::read(root.join(".git/index")).expect("replacement index reads"),
            replacement_index
        );
    }

    #[test]
    fn stage_rejects_an_index_over_the_entry_budget_before_staging() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        plant_over_budget_index(&repository);
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Stage(GitStageArguments {
                paths: vec![TRACKED_PATH.to_owned()],
            }))
            .expect_err("oversized index rejects staging");

        assert_eq!(failure, LocalGitFailure::Operation);
    }

    #[test]
    fn pinned_object_database_never_reopens_a_replacement_fifo() {
        let fixture = Fixture::new();
        let executor = fixture.executor();
        let pinned = PinnedObjectDatabase::capture(&executor.repository_authority)
            .expect("fixture object database pins");
        let object = fixture.initial.to_string();
        let object_path = fixture
            .root()
            .join(".git/objects")
            .join(&object[..2])
            .join(&object[2..]);
        fs::rename(&object_path, object_path.with_extension("pinned"))
            .expect("fixture object path retires");
        mkfifoat(CWD, &object_path, Mode::RUSR | Mode::WUSR)
            .expect("replacement object FIFO constructs");
        let repository = executor
            .repository_authority
            .repository()
            .expect("pinned fixture repository opens");
        let object_database = Odb::new().expect("fixture object database constructs");
        pinned
            .add_to(&object_database)
            .expect("pinned objects attach");
        let _mempack = object_database
            .add_new_mempack_backend(1000)
            .expect("fixture memory object backend attaches");
        repository
            .set_odb(&object_database)
            .expect("fixture object database selects");

        let commit = find_bounded_commit(&repository, fixture.initial)
            .expect("pinned commit remains readable");

        assert_eq!(commit.id(), fixture.initial);
    }

    #[test]
    fn pinned_object_database_snapshots_mutable_pack_contents() {
        let fixture = Fixture::new();
        let executor = fixture.executor();
        let name = "pack-fixture.pack";
        let source = fixture.root().join(".git/objects/pack").join(name);
        let trusted = b"trusted-pack";
        let replacement = b"changed-pack";
        fs::write(&source, trusted).expect("fixture pack writes");
        let pinned = PinnedObjectDatabase::capture(&executor.repository_authority)
            .expect("fixture object database snapshots");

        fs::write(&source, replacement).expect("fixture pack mutates in place");
        let snapshot = fs::read(pinned.directory.path().join("pack").join(name))
            .expect("private pack snapshot reads");

        assert_eq!(snapshot, trusted);
        assert_eq!(
            fs::read(source).expect("mutated source pack reads"),
            replacement
        );
    }

    #[test]
    fn pinned_object_database_admits_one_pack_within_the_aggregate_budget() {
        let fixture = Fixture::new();
        let name = "pack-within-aggregate.pack";
        let pack_bytes = 65 * MAX_OBJECT_BYTES;
        plant_sparse_pack(fixture.root(), name, pack_bytes as u64);
        let executor = fixture.executor();

        let pinned = PinnedObjectDatabase::capture(&executor.repository_authority)
            .expect("single in-budget pack snapshots");
        let captured_bytes = fs::metadata(pinned.directory.path().join("pack").join(name))
            .expect("captured pack metadata reads")
            .len();

        assert_eq!(captured_bytes, pack_bytes as u64);
    }

    #[test]
    fn packed_object_install_rejects_same_length_replacement_content() {
        let source = tempfile::tempdir().expect("fixture source directory constructs");
        let destination = tempfile::tempdir().expect("fixture pack directory constructs");
        let name = "pack-fixture.pack";
        let source_path = source.path().join(name);
        let destination_path = destination.path().join(name);
        fs::write(&source_path, b"trusted").expect("fixture source pack writes");
        fs::write(&destination_path, b"hostile").expect("fixture replacement pack writes");
        let directory = openat(
            CWD,
            destination.path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("fixture pack directory pins");

        let mode = pack_installation_mode(&directory).expect("fixture pack mode resolves");
        let failure = install_packed_object_file(&directory, &source_path, mode)
            .expect_err("different same-length pack rejects");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            fs::read(destination_path).expect("fixture replacement pack reads"),
            b"hostile"
        );
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
    fn stage_preserves_a_monolithic_index_resolve_undo_extension() {
        let fixture = Fixture::new();
        install_deleted_conflict(&fixture);
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("resolved fixture file writes");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let mut index = repository.index().expect("fixture index opens");
        index
            .add_path(Path::new(TRACKED_PATH))
            .expect("fixture conflict resolves");
        index.write().expect("fixture resolve-undo index writes");
        let expected_extension = index_extension(
            &fs::read(fixture.root().join(".git/index")).expect("fixture index reads"),
            b"REUC",
        );
        fs::write(fixture.root().join(UNTRACKED_PATH), UNTRACKED_CONTENT)
            .expect("unrelated fixture file writes");
        let executor = fixture.executor();

        execute(
            &executor,
            LocalOperation::Stage(GitStageArguments {
                paths: vec![UNTRACKED_PATH.to_owned()],
            }),
        );
        let observed_extension = index_extension(
            &fs::read(fixture.root().join(".git/index")).expect("staged index reads"),
            b"REUC",
        );

        assert_eq!(observed_extension, expected_extension);
    }

    #[test]
    fn index_lock_rejects_a_replaced_lock_path_without_touching_it() {
        let fixture = Fixture::new();
        let index_path = fixture.root().join(".git/index");
        let lock_path = fixture.root().join(".git/index.lock");
        let (mut index_lock, mut index) =
            IndexLock::acquire(&index_path, &lock_path).expect("fixture index lock acquires");
        fs::remove_file(&lock_path).expect("owned fixture lock unlinks");
        mkfifoat(CWD, &lock_path, Mode::RUSR | Mode::WUSR)
            .expect("replacement lock FIFO constructs");

        index_lock
            .write(&mut index)
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
    fn repeated_commits_pack_only_objects_created_by_each_operation() {
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
        execute(
            &executor,
            LocalOperation::Commit(GitCommitArguments {
                message: MODEL_MESSAGE.to_owned(),
            }),
        );
        execute(
            &executor,
            LocalOperation::Commit(GitCommitArguments {
                message: INITIAL_MESSAGE.to_owned(),
            }),
        );
        execute(
            &executor,
            LocalOperation::Commit(GitCommitArguments {
                message: CHANGED_CONTENT.to_owned(),
            }),
        );

        assert_eq!(packed_object_counts(fixture.root()), vec![1, 1, 1, 2]);
    }

    #[test]
    fn commit_revalidates_the_injected_root_before_reference_publication() {
        let parent = tempfile::tempdir().expect("workspace parent constructs");
        let root = parent.path().join("workspace");
        let retired = parent.path().join("retired");
        fs::create_dir(&root).expect("workspace root constructs");
        let original = Repository::init(&root).expect("original repository initializes");
        fs::write(root.join(TRACKED_PATH), INITIAL_CONTENT).expect("original fixture file writes");
        let original_head = commit_all(&original, INITIAL_MESSAGE);
        let executor = LocalGitTools::try_new(LocalWorkspaceFileSystem, &root, identity())
            .expect("local Git suite constructs")
            .into_parts()
            .1;
        fs::write(root.join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("original fixture change writes");
        execute(
            &executor,
            LocalOperation::Stage(GitStageArguments {
                paths: vec![TRACKED_PATH.to_owned()],
            }),
        );
        let mut repository = executor
            .repository_authority
            .repository()
            .expect("pinned original repository opens");
        let pinned_objects = PinnedObjectDatabase::capture(&executor.repository_authority)
            .expect("fixture objects pin");
        let persistent_object_database =
            Odb::new().expect("fixture persistent object database constructs");
        pinned_objects
            .add_to(&persistent_object_database)
            .expect("fixture persistent objects attach");
        let object_database = Odb::new().expect("fixture object database constructs");
        pinned_objects
            .add_to(&object_database)
            .expect("fixture objects attach");
        let _mempack = object_database
            .add_new_mempack_backend(1000)
            .expect("fixture memory pack attaches");
        repository
            .set_odb(&object_database)
            .expect("fixture object database installs");
        let mut replacement_head = None;

        let failure = commit(
            &mut repository,
            &executor.identity,
            GitCommitArguments {
                message: MODEL_MESSAGE.to_owned(),
            },
            &executor.repository_authority,
            &persistent_object_database,
            &object_database,
            || {
                fs::rename(&root, &retired).expect("original workspace retires");
                fs::create_dir(&root).expect("replacement workspace constructs");
                let replacement =
                    Repository::init(&root).expect("replacement repository initializes");
                fs::write(root.join(TRACKED_PATH), INITIAL_CONTENT)
                    .expect("replacement fixture file writes");
                replacement_head.replace(commit_all(&replacement, INITIAL_MESSAGE));
                executor.validate_current_repository_identity()
            },
        )
        .expect_err("root replacement rejects before commit publication");
        let replacement = Repository::open(&root).expect("replacement repository opens");
        let retired_repository =
            Repository::open(&retired).expect("retired original repository opens");

        assert_eq!(failure, LocalGitFailure::Repository);
        assert_eq!(
            replacement
                .head()
                .expect("replacement HEAD exists")
                .target(),
            replacement_head
        );
        assert_eq!(
            retired_repository
                .head()
                .expect("retired original HEAD exists")
                .target(),
            Some(original_head)
        );
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
    fn commit_creates_reflogs_with_shared_reference_modes() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("fixture change writes");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let mut index = repository.index().expect("fixture index opens");
        index
            .add_path(Path::new(TRACKED_PATH))
            .expect("fixture change stages");
        index.write().expect("fixture index writes");
        let branch = repository
            .head()
            .expect("fixture HEAD exists")
            .name()
            .expect("fixture branch is UTF-8")
            .to_owned();
        let shared_refs_mode = 0o2770;
        let expected_directory_mode = (shared_refs_mode & 0o2777) | 0o700;
        let expected_file_mode = (shared_refs_mode & 0o666) | 0o600;
        fs::set_permissions(
            fixture.root().join(".git/refs"),
            fs::Permissions::from_mode(shared_refs_mode),
        )
        .expect("fixture shared refs permissions set");
        fs::remove_dir_all(fixture.root().join(".git/logs"))
            .expect("fixture reflog hierarchy removes");
        let executor = fixture.executor();

        execute(
            &executor,
            LocalOperation::Commit(GitCommitArguments {
                message: MODEL_MESSAGE.to_owned(),
            }),
        );
        let logs = fixture.root().join(".git/logs");
        let branch_log = logs.join(branch);
        let logs_mode = fs::metadata(&logs)
            .expect("created logs metadata reads")
            .permissions()
            .mode()
            & 0o2777;
        let branch_parent_mode = fs::metadata(
            branch_log
                .parent()
                .expect("created branch reflog has a parent"),
        )
        .expect("created branch reflog parent metadata reads")
        .permissions()
        .mode()
            & 0o2777;
        let head_log_mode = fs::metadata(logs.join("HEAD"))
            .expect("created HEAD reflog metadata reads")
            .permissions()
            .mode()
            & 0o777;
        let branch_log_mode = fs::metadata(branch_log)
            .expect("created branch reflog metadata reads")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(logs_mode, expected_directory_mode);
        assert_eq!(branch_parent_mode, expected_directory_mode);
        assert_eq!(head_log_mode, expected_file_mode);
        assert_eq!(branch_log_mode, expected_file_mode);
    }

    #[test]
    fn commit_creates_a_missing_reference_with_shared_modes() {
        let fixture = Fixture::new();
        let refs = fixture.root().join(".git/refs");
        let shared_refs_mode = 0o2770;
        let expected_directory_mode = (shared_refs_mode & 0o2777) | 0o700;
        let expected_file_mode = (shared_refs_mode & 0o666) | 0o600;
        fs::set_permissions(&refs, fs::Permissions::from_mode(shared_refs_mode))
            .expect("fixture shared refs permissions set");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        repository
            .set_head("refs/heads/shared/topic/fix")
            .expect("missing fixture branch selects");
        let executor = fixture.executor();

        execute(
            &executor,
            LocalOperation::Commit(GitCommitArguments {
                message: MODEL_MESSAGE.to_owned(),
            }),
        );
        let created_directory_mode = fs::metadata(refs.join("heads/shared/topic"))
            .expect("created reference directory metadata reads")
            .permissions()
            .mode()
            & 0o7777;
        let created_file_mode = fs::metadata(refs.join("heads/shared/topic/fix"))
            .expect("created reference metadata reads")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(created_directory_mode, expected_directory_mode);
        assert_eq!(created_file_mode, expected_file_mode);
    }

    #[test]
    fn commit_rejects_a_reflog_fifo_without_blocking_or_advancing() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("fixture change writes");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let mut index = repository.index().expect("fixture index opens");
        index
            .add_path(Path::new(TRACKED_PATH))
            .expect("fixture change stages");
        index.write().expect("fixture index writes");
        let branch = repository
            .head()
            .expect("fixture HEAD exists")
            .name()
            .expect("fixture branch is UTF-8")
            .to_owned();
        let branch_log = fixture.root().join(".git/logs").join(branch);
        let executor = fixture.executor();
        fs::remove_file(&branch_log).expect("fixture branch reflog removes");
        mkfifoat(CWD, &branch_log, Mode::RUSR | Mode::WUSR)
            .expect("fixture branch reflog FIFO constructs");

        let rejected_without_wait =
            commit_rejects_reflog_without_wait(executor, branch_log.clone());

        assert!(rejected_without_wait);
        assert!(
            fs::symlink_metadata(branch_log)
                .expect("fixture reflog metadata reads")
                .file_type()
                .is_fifo()
        );
        assert_eq!(
            repository.head().expect("HEAD exists").target(),
            Some(fixture.initial)
        );
    }

    #[test]
    fn commit_rejects_a_multiply_linked_reflog_without_mutating_outside() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("fixture change writes");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let mut index = repository.index().expect("fixture index opens");
        index
            .add_path(Path::new(TRACKED_PATH))
            .expect("fixture change stages");
        index.write().expect("fixture index writes");
        let branch = repository
            .head()
            .expect("fixture HEAD exists")
            .name()
            .expect("fixture branch is UTF-8")
            .to_owned();
        let branch_log = fixture.root().join(".git/logs").join(branch);
        fs::remove_file(&branch_log).expect("fixture branch reflog removes");
        let outside = tempfile::tempdir().expect("outside directory constructs");
        let outside_log = outside.path().join("outside.log");
        let outside_content = b"outside reflog remains exact\n";
        fs::write(&outside_log, outside_content).expect("outside reflog writes");
        fs::hard_link(&outside_log, &branch_log)
            .expect("outside reflog hard-links into repository");
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Commit(GitCommitArguments {
                message: MODEL_MESSAGE.to_owned(),
            }))
            .expect_err("multiply linked reflog rejects commit");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            fs::read(outside_log).expect("outside reflog reads"),
            outside_content
        );
        assert_eq!(
            repository.head().expect("HEAD exists").target(),
            Some(fixture.initial)
        );
    }

    #[test]
    fn commit_preserves_the_existing_loose_reference_permissions() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("fixture change writes");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let mut index = repository.index().expect("fixture index opens");
        index
            .add_path(Path::new(TRACKED_PATH))
            .expect("fixture change stages");
        index.write().expect("fixture index writes");
        let branch = repository
            .head()
            .expect("fixture HEAD exists")
            .name()
            .expect("fixture branch is UTF-8")
            .to_owned();
        let branch_path = fixture.root().join(".git").join(branch);
        let expected_mode = 0o640;
        fs::set_permissions(&branch_path, fs::Permissions::from_mode(expected_mode))
            .expect("fixture reference permissions set");
        let executor = fixture.executor();

        execute(
            &executor,
            LocalOperation::Commit(GitCommitArguments {
                message: MODEL_MESSAGE.to_owned(),
            }),
        );
        let updated_mode = fs::metadata(branch_path)
            .expect("updated reference metadata reads")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(updated_mode, expected_mode);
    }

    #[test]
    fn commit_publishes_reflogs_while_the_target_reference_lock_is_held() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let tree = repository
            .find_commit(fixture.initial)
            .expect("fixture commit opens")
            .tree_id();
        let new = raw_commit_with_tree(&repository, tree, fixture.initial);
        let executor = fixture.executor();
        let (chain, old) = resolve_pinned_reference_chain(&executor.repository_authority, None)
            .expect("fixture reference chain resolves");
        let update_reference = chain.last().expect("fixture branch target exists");
        let update_lock = ReferenceLock::acquire(&executor.repository_authority, update_reference)
            .expect("fixture target reference locks");
        let reference_path = fixture.root().join(".git").join(update_reference);
        let lock_path = reference_path.with_extension("lock");
        let head_log = fixture.root().join(".git/logs/HEAD");
        let branch_log = fixture.root().join(".git/logs").join(update_reference);
        let signature = identity()
            .signature()
            .expect("fixture signature constructs");

        publish_commit_reference_with_hook(
            &executor.repository_authority,
            update_lock,
            update_reference,
            old.expect("fixture parent exists"),
            new,
            &signature,
            || {
                assert!(lock_path.exists());
                assert_eq!(
                    fs::read_to_string(&reference_path).expect("locked reference reads"),
                    format!("{}\n", fixture.initial)
                );
                assert!(
                    fs::read_to_string(&head_log)
                        .expect("published HEAD reflog reads")
                        .contains(&new.to_string())
                );
                assert!(
                    fs::read_to_string(&branch_log)
                        .expect("published branch reflog reads")
                        .contains(&new.to_string())
                );
            },
        )
        .expect("fixture reference and reflogs publish");

        assert_eq!(
            fs::read_to_string(reference_path).expect("published reference reads"),
            format!("{new}\n")
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
    fn commit_rejects_an_unborn_branch_beneath_a_packed_reference() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let packed_reference = "refs/heads/release";
        let unborn_reference = "refs/heads/release/v1";
        repository
            .set_head(unborn_reference)
            .expect("unborn fixture branch selects");
        fs::write(
            fixture.root().join(".git/packed-refs"),
            format!("{} {}\n", fixture.initial, packed_reference),
        )
        .expect("packed ancestor fixture writes");
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Commit(GitCommitArguments {
                message: MODEL_MESSAGE.to_owned(),
            }))
            .expect_err("packed ancestor rejects unborn commit");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert!(!fixture.root().join(".git/refs/heads/release").exists());
        assert_eq!(
            repository
                .find_reference("HEAD")
                .expect("symbolic HEAD remains")
                .symbolic_target(),
            Ok(Some(unborn_reference))
        );
    }

    #[test]
    fn failed_unborn_commit_removes_its_new_reference_directories() {
        let root = tempfile::tempdir().expect("temporary repository root constructs");
        let repository = Repository::init(root.path()).expect("unborn repository initializes");
        let unborn_reference = "refs/heads/topic/v1";
        repository
            .set_head(unborn_reference)
            .expect("nested unborn branch selects");
        fs::write(root.path().join(TRACKED_PATH), INITIAL_CONTENT).expect("fixture file writes");
        let executor = LocalGitTools::try_new(LocalWorkspaceFileSystem, root.path(), identity())
            .expect("local Git suite constructs")
            .into_parts()
            .1;
        execute(
            &executor,
            LocalOperation::Stage(GitStageArguments {
                paths: vec![TRACKED_PATH.to_owned()],
            }),
        );
        let mut pinned_repository = executor
            .repository_authority
            .repository()
            .expect("pinned unborn repository opens");
        let pinned_objects = PinnedObjectDatabase::capture(&executor.repository_authority)
            .expect("fixture objects pin");
        let persistent_object_database =
            Odb::new().expect("fixture persistent object database constructs");
        pinned_objects
            .add_to(&persistent_object_database)
            .expect("fixture persistent objects attach");
        let object_database = Odb::new().expect("fixture object database constructs");
        pinned_objects
            .add_to(&object_database)
            .expect("fixture writable objects attach");
        let _mempack = object_database
            .add_new_mempack_backend(1000)
            .expect("fixture memory pack attaches");
        pinned_repository
            .set_odb(&object_database)
            .expect("fixture writable object database installs");

        let failure = commit(
            &mut pinned_repository,
            &executor.identity,
            GitCommitArguments {
                message: MODEL_MESSAGE.to_owned(),
            },
            &executor.repository_authority,
            &persistent_object_database,
            &object_database,
            || Err(LocalGitFailure::Repository),
        )
        .expect_err("final validation rejects unborn commit");

        assert_eq!(failure, LocalGitFailure::Repository);
        assert!(!root.path().join(".git/refs/heads/topic").exists());
        assert_eq!(
            repository
                .find_reference("HEAD")
                .expect("symbolic HEAD remains")
                .symbolic_target(),
            Ok(Some(unborn_reference))
        );
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
    fn commit_rejects_indexed_blob_bytes_over_the_tree_budget() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        plant_index_over_blob_budget(&repository);
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Commit(GitCommitArguments {
                message: MODEL_MESSAGE.to_owned(),
            }))
            .expect_err("aggregate indexed blobs reject before packing");

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
    fn commit_reference_publication_rejects_a_replaced_refs_hierarchy() {
        let fixture = Fixture::new();
        let executor = fixture.executor();
        let (chain, _) = resolve_pinned_reference_chain(&executor.repository_authority, None)
            .expect("fixture reference chain resolves");
        let mut locks = chain
            .iter()
            .map(|name| ReferenceLock::acquire(&executor.repository_authority, name))
            .collect::<Result<Vec<_>, _>>()
            .expect("fixture reference locks acquire");
        let target_name = chain
            .last()
            .expect("fixture branch target exists")
            .to_owned();
        let target_position = locks
            .iter()
            .position(|lock| lock.name == target_name)
            .expect("fixture branch lock exists");
        let target_lock = locks.swap_remove(target_position);
        let retired_refs = fixture.root().join(".git/refs-retired");
        fs::rename(fixture.root().join(".git/refs"), &retired_refs).expect("fixture refs retire");
        let outside = tempfile::tempdir().expect("outside refs root constructs");
        fs::create_dir(outside.path().join("heads")).expect("outside heads directory constructs");
        symlink(outside.path(), fixture.root().join(".git/refs"))
            .expect("replacement refs symlink constructs");

        let failure = target_lock
            .commit(&executor.repository_authority, fixture.initial)
            .expect_err("replacement refs hierarchy rejects publication");
        let relative_target = target_name
            .strip_prefix("refs/")
            .expect("fixture target is under refs");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            fs::read_to_string(retired_refs.join(relative_target))
                .expect("retired fixture branch reads"),
            format!("{}\n", fixture.initial)
        );
        assert!(!outside.path().join(relative_target).exists());
        drop(locks);
        fs::remove_file(fixture.root().join(".git/refs"))
            .expect("replacement refs symlink removes");
        fs::rename(retired_refs, fixture.root().join(".git/refs")).expect("fixture refs restore");
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
        let pinned_objects = PinnedObjectDatabase::capture(&executor.repository_authority)
            .expect("fixture objects pin");
        let persistent_object_database =
            Odb::new().expect("fixture persistent object database constructs");
        pinned_objects
            .add_to(&persistent_object_database)
            .expect("fixture persistent objects attach");
        let object_database = Odb::new().expect("fixture object database constructs");
        pinned_objects
            .add_to(&object_database)
            .expect("fixture pinned objects attach");
        let _mempack = object_database
            .add_new_mempack_backend(1000)
            .expect("fixture mempack attaches");
        repository
            .set_odb(&object_database)
            .expect("fixture repository uses pinned objects");
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
            &executor.repository_authority,
            &persistent_object_database,
            &object_database,
            || executor.validate_current_repository_identity(),
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
    fn stage_records_child_deletion_when_its_parent_becomes_a_file() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.root().join(NESTED_TRACKED_DIRECTORY))
            .expect("nested fixture directory constructs");
        fs::write(fixture.root().join(NESTED_TRACKED_PATH), INITIAL_CONTENT)
            .expect("nested tracked file writes");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        commit_all(&repository, INITIAL_MESSAGE);
        fs::remove_dir_all(fixture.root().join(NESTED_TRACKED_DIRECTORY))
            .expect("tracked parent directory removes");
        fs::write(
            fixture.root().join(NESTED_TRACKED_DIRECTORY),
            CHANGED_CONTENT,
        )
        .expect("replacement parent file writes");
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
            || Ok(()),
        )
        .expect_err("replaced refs hierarchy rejects");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert!(!outside.path().join("heads/agent/fix").exists());
    }

    #[test]
    fn branch_create_revalidates_the_injected_root_before_publication() {
        let parent = tempfile::tempdir().expect("workspace parent constructs");
        let root = parent.path().join("workspace");
        let retired = parent.path().join("retired");
        fs::create_dir(&root).expect("workspace root constructs");
        let original = Repository::init(&root).expect("original repository initializes");
        fs::write(root.join(TRACKED_PATH), INITIAL_CONTENT).expect("original fixture file writes");
        let initial = commit_all(&original, INITIAL_MESSAGE);
        let executor = LocalGitTools::try_new(LocalWorkspaceFileSystem, &root, identity())
            .expect("local Git suite constructs")
            .into_parts()
            .1;
        let repository = executor
            .repository_authority
            .repository()
            .expect("pinned original repository opens");

        let failure = branch_create(
            &repository,
            &executor.repository_authority,
            GitBranchCreateArguments {
                name: FIX_BRANCH.to_owned(),
                start: initial.to_string(),
            },
            || {
                fs::rename(&root, &retired).expect("original workspace retires");
                fs::create_dir(&root).expect("replacement workspace constructs");
                Repository::init(&root).expect("replacement repository initializes");
                executor.validate_current_repository_identity()
            },
        )
        .expect_err("replaced root rejects branch publication");

        assert_eq!(failure, LocalGitFailure::Repository);
        assert!(!retired.join(".git/refs/heads/agent/fix").exists());
        assert!(!root.join(".git/refs/heads/agent/fix").exists());
    }

    #[test]
    fn branch_create_rejects_a_packed_descendant_reference() {
        let fixture = Fixture::new();
        fs::write(
            fixture.root().join(".git/packed-refs"),
            format!(
                "# pack-refs with: peeled fully-peeled sorted\n{} refs/heads/release/v1\n",
                fixture.initial
            ),
        )
        .expect("fixture packed reference writes");
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::BranchCreate(GitBranchCreateArguments {
                name: "release".to_owned(),
                start: fixture.initial.to_string(),
            }))
            .expect_err("packed descendant reference rejects");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert!(!fixture.root().join(".git/refs/heads/release").exists());
    }

    #[test]
    fn branch_create_rechecks_packed_references_after_pinning_loose_hierarchy() {
        let fixture = Fixture::new();
        let executor = fixture.executor();
        let packed_references = fixture.root().join(".git/packed-refs");

        let failure = create_loose_branch_reference_with_hook(
            &executor.repository_authority,
            "release/v1",
            fixture.initial,
            || {
                fs::write(
                    &packed_references,
                    format!(
                        "# pack-refs with: peeled fully-peeled sorted\n{} refs/heads/release\n",
                        fixture.initial
                    ),
                )
                .expect("racing packed reference writes");
            },
        )
        .expect_err("packed ancestor appearing before publication rejects");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert!(!fixture.root().join(".git/refs/heads/release/v1").exists());
    }

    #[test]
    fn branch_create_uses_shared_reference_hierarchy_modes() {
        let fixture = Fixture::new();
        let refs = fixture.root().join(".git/refs");
        let shared_refs_mode = 0o2770;
        let expected_directory_mode = (shared_refs_mode & 0o2777) | 0o700;
        let expected_file_mode = (shared_refs_mode & 0o666) | 0o600;
        fs::set_permissions(&refs, fs::Permissions::from_mode(shared_refs_mode))
            .expect("fixture shared refs permissions set");
        let executor = fixture.executor();

        execute(
            &executor,
            LocalOperation::BranchCreate(GitBranchCreateArguments {
                name: "shared/topic/fix".to_owned(),
                start: fixture.initial.to_string(),
            }),
        );
        let created_directory_mode = fs::metadata(refs.join("heads/shared/topic"))
            .expect("created reference directory metadata reads")
            .permissions()
            .mode()
            & 0o7777;
        let created_file_mode = fs::metadata(refs.join("heads/shared/topic/fix"))
            .expect("created reference metadata reads")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(created_directory_mode, expected_directory_mode);
        assert_eq!(created_file_mode, expected_file_mode);
    }

    #[test]
    fn branch_create_rejects_a_replaced_lock_without_touching_it() {
        let fixture = Fixture::new();
        let executor = fixture.executor();
        let lock_path = fixture.root().join(".git/refs/heads/race.lock");
        let retired_lock = fixture.root().join(".git/refs/heads/race.lock.pinned");

        let failure = create_loose_branch_reference_with_hook(
            &executor.repository_authority,
            "race",
            fixture.initial,
            || {
                fs::rename(&lock_path, &retired_lock).expect("fixture branch lock retires");
                fs::write(&lock_path, b"replacement\n").expect("replacement branch lock writes");
            },
        )
        .expect_err("replaced branch lock rejects");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            fs::read(&lock_path).expect("replacement branch lock reads"),
            b"replacement\n"
        );
        assert!(!fixture.root().join(".git/refs/heads/race").exists());
        fs::remove_file(lock_path).expect("replacement branch lock removes");
        fs::remove_file(retired_lock).expect("retired branch lock removes");
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
    fn branch_switch_does_not_create_a_hierarchy_for_an_absent_branch() {
        let fixture = Fixture::new();
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::BranchSwitch(GitBranchSwitchArguments {
                name: "missing/topic".to_owned(),
            }))
            .expect_err("absent nested branch rejects");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert!(!fixture.root().join(".git/refs/heads/missing").exists());
    }

    #[test]
    fn branch_switch_preserves_a_modified_tracked_file_when_safe_checkout_refuses() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let initial = repository
            .find_commit(fixture.initial)
            .expect("fixture commit exists");
        repository
            .branch(FIX_BRANCH, &initial, false)
            .expect("fixture branch creates");
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("current branch fixture change writes");
        commit_all(&repository, MODEL_MESSAGE);
        fs::write(fixture.root().join(TRACKED_PATH), TARGET_CONTENT)
            .expect("local tracked modification writes");
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::BranchSwitch(GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            }))
            .expect_err("safe checkout rejects the local tracked modification");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            fs::read(fixture.root().join(TRACKED_PATH)).expect("local modification reads"),
            TARGET_CONTENT.as_bytes()
        );
    }

    #[test]
    fn branch_switch_preserves_an_untracked_obstruction_when_safe_checkout_refuses() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let initial_tree = repository
            .find_commit(fixture.initial)
            .expect("fixture initial commit exists")
            .tree()
            .expect("fixture initial tree opens");
        let target_blob = repository
            .blob(UNTRACKED_CONTENT.as_bytes())
            .expect("target fixture blob writes");
        let mut target_builder = repository
            .treebuilder(Some(&initial_tree))
            .expect("target fixture tree builder opens");
        target_builder
            .insert(UNTRACKED_PATH, target_blob, 0o100644)
            .expect("target fixture path inserts");
        let target_tree = target_builder.write().expect("target fixture tree writes");
        let target = raw_commit_with_tree(&repository, target_tree, fixture.initial);
        let target = repository
            .find_commit(target)
            .expect("target fixture commit exists");
        repository
            .branch(FIX_BRANCH, &target, false)
            .expect("target fixture branch creates");
        fs::write(fixture.root().join(UNTRACKED_PATH), TARGET_CONTENT)
            .expect("untracked obstruction writes");
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::BranchSwitch(GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            }))
            .expect_err("safe checkout rejects the untracked obstruction");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            fs::read(fixture.root().join(UNTRACKED_PATH)).expect("untracked obstruction reads"),
            TARGET_CONTENT.as_bytes()
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
    fn branch_switch_allows_a_clean_file_to_directory_transition() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let initial_tree = repository
            .find_commit(fixture.initial)
            .expect("fixture initial commit exists")
            .tree()
            .expect("fixture initial tree opens");
        let nested_blob = repository
            .blob(b"nested\n")
            .expect("nested fixture blob writes");
        let mut nested_builder = repository
            .treebuilder(None)
            .expect("nested fixture tree builder opens");
        nested_builder
            .insert("main.txt", nested_blob, 0o100644)
            .expect("nested fixture blob inserts");
        let nested_tree = nested_builder.write().expect("nested fixture tree writes");
        let mut target_builder = repository
            .treebuilder(Some(&initial_tree))
            .expect("target fixture tree builder opens");
        target_builder
            .insert("src", nested_tree, 0o040000)
            .expect("target fixture directory inserts");
        let target_tree = target_builder.write().expect("target fixture tree writes");
        let target = raw_commit_with_tree(&repository, target_tree, fixture.initial);
        let target = repository
            .find_commit(target)
            .expect("target fixture commit exists");
        repository
            .branch(FIX_BRANCH, &target, false)
            .expect("target fixture branch creates");
        fs::write(fixture.root().join("src"), b"flat\n").expect("flat fixture file writes");
        commit_all(&repository, "flat source");
        let executor = fixture.executor();

        execute(
            &executor,
            LocalOperation::BranchSwitch(GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            }),
        );

        assert_eq!(
            fs::read(fixture.root().join("src/main.txt")).expect("nested fixture content reads"),
            b"nested\n"
        );
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
    fn branch_switch_rejects_target_tree_blob_bytes_over_the_aggregate_budget() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let oversized = aggregate_blob_tree_commit(&repository, fixture.initial);
        let oversized = repository
            .find_commit(oversized)
            .expect("aggregate-tree fixture commit exists");
        repository
            .branch(FIX_BRANCH, &oversized, false)
            .expect("aggregate-tree fixture branch creates");
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
            .expect_err("aggregate target-tree bytes reject");

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
    fn branch_switch_preserves_assume_valid_on_an_unchanged_path() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let initial = repository
            .find_commit(fixture.initial)
            .expect("fixture initial commit exists");
        repository
            .branch(FIX_BRANCH, &initial, false)
            .expect("fixture branch creates");
        fs::write(fixture.root().join("branch-only.txt"), CHANGED_CONTENT)
            .expect("current-branch fixture file writes");
        commit_all(&repository, MODEL_MESSAGE);
        let mut index = repository.index().expect("fixture index opens");
        let mut entry = clone_index_entry(
            &index
                .get_path(Path::new(TRACKED_PATH), 0)
                .expect("unchanged fixture entry exists"),
        );
        entry.flags |= INDEX_ASSUME_VALID;
        index
            .add(&entry)
            .expect("assume-valid fixture entry installs");
        index.write().expect("assume-valid fixture index writes");
        let executor = fixture.executor();

        execute(
            &executor,
            LocalOperation::BranchSwitch(GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            }),
        );
        let switched_repository =
            Repository::open(fixture.root()).expect("switched repository reopens");
        let switched_index = switched_repository
            .index()
            .expect("switched fixture index opens");
        let switched_entry = switched_index
            .get_path(Path::new(TRACKED_PATH), 0)
            .expect("unchanged switched entry exists");

        assert_eq!(
            switched_entry.flags & INDEX_ASSUME_VALID,
            entry.flags & INDEX_ASSUME_VALID
        );
    }

    #[test]
    fn status_and_worktree_diff_hide_an_assume_unchanged_worktree_edit() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        set_index_flags(&repository, TRACKED_PATH, INDEX_ASSUME_VALID);
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("assume-unchanged fixture edit writes");
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);
        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

        assert_eq!(status["entries"], serde_json::json!([]));
        assert_eq!(diff["patch"], "");
        assert_eq!(diff["truncated"], false);
    }

    #[test]
    fn status_and_worktree_diff_preserve_an_assume_unchanged_staged_edit() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("staged fixture edit writes");
        let mut index = repository.index().expect("fixture index opens");
        index
            .add_path(Path::new(TRACKED_PATH))
            .expect("fixture edit stages");
        index.write().expect("fixture index writes");
        set_index_flags(&repository, TRACKED_PATH, INDEX_ASSUME_VALID);
        fs::write(fixture.root().join(TRACKED_PATH), INITIAL_CONTENT)
            .expect("assume-unchanged worktree restores");
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);
        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

        assert_eq!(status["entries"][0]["path"], TRACKED_PATH);
        assert_eq!(status["entries"][0]["index"], "modified");
        assert_eq!(status["entries"][0]["worktree"], "unchanged");
        assert!(
            diff["patch"]
                .as_str()
                .expect("fixture patch is text")
                .contains(CHANGED_CONTENT)
        );
    }

    #[test]
    fn branch_switch_rejects_a_staged_path_changed_by_the_target() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let initial_tree = repository
            .find_commit(fixture.initial)
            .expect("fixture initial commit exists")
            .tree()
            .expect("fixture initial tree opens");
        let target_blob = repository
            .blob(TARGET_CONTENT.as_bytes())
            .expect("target fixture blob writes");
        let mut target_builder = repository
            .treebuilder(Some(&initial_tree))
            .expect("target fixture tree builder opens");
        target_builder
            .insert(TRACKED_PATH, target_blob, 0o100644)
            .expect("target fixture blob inserts");
        let target_tree = target_builder.write().expect("target fixture tree writes");
        let target = raw_commit_with_tree(&repository, target_tree, fixture.initial);
        let target = repository
            .find_commit(target)
            .expect("target fixture commit exists");
        repository
            .branch(FIX_BRANCH, &target, false)
            .expect("target fixture branch creates");
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("staged fixture change writes");
        let mut index = repository.index().expect("fixture index opens");
        index
            .add_path(Path::new(TRACKED_PATH))
            .expect("fixture change stages");
        index.write().expect("fixture index writes");
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::BranchSwitch(GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            }))
            .expect_err("target overlap rejects branch switch");
        let index = repository.index().expect("fixture index reopens");
        let entry = index
            .get_path(Path::new(TRACKED_PATH), 0)
            .expect("staged fixture path remains indexed");
        let blob = repository
            .find_blob(entry.id)
            .expect("staged fixture blob remains readable");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            fs::read(fixture.root().join(TRACKED_PATH)).expect("fixture content remains readable"),
            CHANGED_CONTENT.as_bytes()
        );
        assert_eq!(blob.content(), CHANGED_CONTENT.as_bytes());
        assert_eq!(
            repository.head().expect("fixture HEAD remains").target(),
            Some(fixture.initial)
        );
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
    fn branch_switch_rejects_a_replaced_target_reference_directory() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let initial = repository
            .find_commit(fixture.initial)
            .expect("fixture initial commit exists");
        repository
            .branch(FIX_BRANCH, &initial, false)
            .expect("fixture branch creates");
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("current branch fixture change writes");
        let original_head = commit_all(&repository, MODEL_MESSAGE);
        let executor = fixture.executor();
        let outside = tempfile::tempdir().expect("outside reference directory constructs");
        let target_parent = fixture.root().join(".git/refs/heads/agent");
        let retired_parent = fixture.root().join(".git/refs/heads/agent.retired");

        let failure = executor
            .branch_switch_with_reference_lock_hook(
                &executor
                    .repository_authority
                    .repository()
                    .expect("pinned fixture repository opens"),
                GitBranchSwitchArguments {
                    name: FIX_BRANCH.to_owned(),
                },
                || {
                    fs::rename(&target_parent, &retired_parent)
                        .expect("target reference parent retires");
                    symlink(outside.path(), &target_parent)
                        .expect("replacement reference symlink constructs");
                },
            )
            .expect_err("replacement target reference directory rejects");
        fs::remove_file(&target_parent).expect("replacement reference symlink removes");
        fs::rename(&retired_parent, &target_parent).expect("target reference parent restores");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert!(!outside.path().join("fix.lock").exists());
        assert_eq!(
            repository.head().expect("fixture HEAD remains").target(),
            Some(original_head)
        );
    }

    #[test]
    fn branch_switch_rolls_back_checkout_after_index_commit_failure() {
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
        let original_head = commit_all(&repository, MODEL_MESSAGE);
        let executor = fixture.executor();
        let index_lock = fixture.root().join(".git/index.lock");
        let retired_lock = fixture.root().join(".git/index.lock.pinned");

        let failure = executor
            .branch_switch_with_hook(
                &executor
                    .repository_authority
                    .repository()
                    .expect("pinned fixture repository opens"),
                GitBranchSwitchArguments {
                    name: FIX_BRANCH.to_owned(),
                },
                || {
                    fs::rename(&index_lock, &retired_lock).expect("fixture index lock retires");
                    fs::write(&index_lock, []).expect("replacement index lock writes");
                },
            )
            .expect_err("replaced index lock rejects after checkout");
        fs::remove_file(&index_lock).expect("replacement index lock removes");
        fs::remove_file(&retired_lock).expect("retired index lock removes");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            fs::read(fixture.root().join(TRACKED_PATH)).expect("rolled-back fixture content reads"),
            CHANGED_CONTENT.as_bytes()
        );
        assert_eq!(
            repository.head().expect("fixture HEAD remains").target(),
            Some(original_head)
        );
    }

    #[test]
    fn branch_switch_rolls_back_after_target_reference_revalidation_fails() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let initial = repository
            .find_commit(fixture.initial)
            .expect("fixture initial commit exists");
        repository
            .branch(FIX_BRANCH, &initial, false)
            .expect("fixture branch creates");
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("current branch fixture change writes");
        let original_head = commit_all(&repository, MODEL_MESSAGE);
        let executor = fixture.executor();
        let target_reference = fixture.root().join(".git/refs/heads/agent/fix");
        let retired_reference = fixture.root().join(".git/refs/heads/agent/fix.retired");

        let failure = executor
            .branch_switch_with_hook(
                &executor
                    .repository_authority
                    .repository()
                    .expect("pinned fixture repository opens"),
                GitBranchSwitchArguments {
                    name: FIX_BRANCH.to_owned(),
                },
                || {
                    fs::rename(&target_reference, &retired_reference)
                        .expect("target reference retires after checkout");
                    mkfifoat(CWD, &target_reference, Mode::RUSR | Mode::WUSR)
                        .expect("replacement target reference FIFO constructs");
                },
            )
            .expect_err("target reference revalidation rejects after checkout");
        fs::remove_file(&target_reference).expect("replacement target reference FIFO removes");
        fs::rename(&retired_reference, &target_reference).expect("target reference restores");
        let restored_repository =
            Repository::open(fixture.root()).expect("restored fixture repository opens");
        let restored_index = restored_repository
            .index()
            .expect("restored fixture index opens");
        let restored_entry = restored_index
            .get_path(Path::new(TRACKED_PATH), 0)
            .expect("restored tracked entry exists");
        let restored_blob = restored_repository
            .find_blob(restored_entry.id)
            .expect("restored tracked blob opens");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            fs::read(fixture.root().join(TRACKED_PATH)).expect("rolled-back fixture content reads"),
            CHANGED_CONTENT.as_bytes()
        );
        assert_eq!(restored_blob.content(), CHANGED_CONTENT.as_bytes());
        assert_eq!(
            restored_repository
                .head()
                .expect("fixture HEAD remains")
                .target(),
            Some(original_head)
        );
    }

    #[test]
    fn branch_switch_rollback_preserves_a_concurrently_published_index() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let initial = repository
            .find_commit(fixture.initial)
            .expect("fixture initial commit exists");
        repository
            .branch(FIX_BRANCH, &initial, false)
            .expect("fixture branch creates");
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("current branch fixture change writes");
        let original_head = commit_all(&repository, MODEL_MESSAGE);
        let executor = fixture.executor();
        let target_reference = fixture.root().join(".git/refs/heads/agent/fix");
        let retired_reference = fixture.root().join(".git/refs/heads/agent/fix.retired");
        let mut competing_index = Vec::new();

        let failure = executor
            .branch_switch_with_index_publish_hook(
                &executor
                    .repository_authority
                    .repository()
                    .expect("pinned fixture repository opens"),
                GitBranchSwitchArguments {
                    name: FIX_BRANCH.to_owned(),
                },
                || {
                    let competing =
                        Repository::open(fixture.root()).expect("competing repository opens");
                    let blob = competing
                        .blob(UNTRACKED_CONTENT.as_bytes())
                        .expect("competing blob writes");
                    let mut index = competing.index().expect("competing index opens");
                    let mut entry = clone_index_entry(
                        &index
                            .get_path(Path::new(TRACKED_PATH), 0)
                            .expect("competing tracked entry exists"),
                    );
                    entry.id = blob;
                    entry.file_size = UNTRACKED_CONTENT.len() as u32;
                    index.add(&entry).expect("competing entry stages");
                    index.write().expect("competing index publishes");
                    competing_index = fs::read(fixture.root().join(".git/index"))
                        .expect("competing index bytes read");
                    fs::rename(&target_reference, &retired_reference)
                        .expect("target reference retires after index publication");
                    mkfifoat(CWD, &target_reference, Mode::RUSR | Mode::WUSR)
                        .expect("replacement target reference FIFO constructs");
                },
            )
            .expect_err("target reference failure rejects switch");
        fs::remove_file(&target_reference).expect("replacement target reference FIFO removes");
        fs::rename(&retired_reference, &target_reference).expect("target reference restores");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            fs::read(fixture.root().join(".git/index")).expect("published index reads"),
            competing_index
        );
        assert_eq!(
            repository.head().expect("fixture HEAD remains").target(),
            Some(original_head)
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
    fn status_head_snapshot_does_not_mix_a_later_head_selection() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let captured_head = repository
            .find_reference("HEAD")
            .expect("fixture HEAD captures");
        let captured_branch = captured_head
            .symbolic_target()
            .expect("fixture HEAD is symbolic")
            .expect("fixture HEAD has a target")
            .strip_prefix("refs/heads/")
            .expect("fixture HEAD targets a local branch")
            .to_owned();
        let initial_tree = repository
            .find_commit(fixture.initial)
            .expect("fixture initial commit opens")
            .tree_id();
        let replacement = raw_commit_with_tree(&repository, initial_tree, fixture.initial);
        let replacement = repository
            .find_commit(replacement)
            .expect("replacement commit opens");
        repository
            .branch(FIX_BRANCH, &replacement, false)
            .expect("replacement branch creates");
        repository
            .set_head(&format!("refs/heads/{FIX_BRANCH}"))
            .expect("replacement HEAD selects");

        let (branch, truncated, head) =
            status_head_from_reference(&captured_head).expect("captured HEAD resolves");

        assert_eq!(branch.as_deref(), Some(captured_branch.as_str()));
        assert!(!truncated);
        assert_eq!(head, Some(fixture.initial));
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
    fn status_and_worktree_diff_read_tracked_symlink_targets_without_following() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let link_path = "tracked-link";
        let initial_target = "first-target";
        let changed_target = "second-target";
        symlink(initial_target, fixture.root().join(link_path))
            .expect("tracked fixture symlink creates");
        commit_all(&repository, MODEL_MESSAGE);
        let executor = fixture.executor();

        let clean_status = execute(&executor, LocalOperation::Status);
        let clean_diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
        fs::remove_file(fixture.root().join(link_path)).expect("fixture symlink removes");
        symlink(changed_target, fixture.root().join(link_path))
            .expect("changed fixture symlink creates");
        let changed_status = execute(&executor, LocalOperation::Status);
        let changed_diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
        let patch = changed_diff["patch"]
            .as_str()
            .expect("changed symlink patch is text");

        assert_eq!(clean_status["entries"], serde_json::json!([]));
        assert_eq!(clean_diff["patch"], "");
        assert_eq!(changed_status["entries"][0]["path"], link_path);
        assert_eq!(changed_status["entries"][0]["worktree"], "modified");
        assert!(patch.contains(initial_target));
        assert!(patch.contains(changed_target));
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
    fn status_and_worktree_diff_report_a_missing_tracked_gitlink_as_deleted() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        install_gitlink(&repository, SUBMODULE_PATH, fixture.initial);
        commit_index(&repository, MODEL_MESSAGE);
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);
        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
        let patch = diff["patch"].as_str().expect("fixture patch is text");

        assert_eq!(status["entries"][0]["path"], SUBMODULE_PATH);
        assert_eq!(status["entries"][0]["worktree"], "deleted");
        assert!(patch.contains("deleted file mode 160000"));
        assert!(patch.contains(&format!("Subproject commit {}", fixture.initial)));
    }

    #[test]
    fn status_and_worktree_diff_report_a_tracked_gitlink_replaced_by_a_file() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        install_gitlink(&repository, SUBMODULE_PATH, fixture.initial);
        commit_index(&repository, MODEL_MESSAGE);
        fs::write(fixture.root().join(SUBMODULE_PATH), UNTRACKED_CONTENT)
            .expect("replacement fixture file writes");
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);
        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
        let patch = diff["patch"].as_str().expect("fixture patch is text");

        assert_eq!(status["entries"][0]["path"], SUBMODULE_PATH);
        assert_eq!(status["entries"][0]["worktree"], "type_changed");
        assert!(patch.contains("old mode 160000"));
        assert!(patch.contains("new mode 100644"));
        assert!(patch.contains(UNTRACKED_CONTENT));
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
    fn inline_configured_external_ignore_file_is_rejected() {
        let fixture = Fixture::new();
        let mut config = fs::OpenOptions::new()
            .append(true)
            .open(fixture.root().join(".git/config"))
            .expect("fixture config opens");
        writeln!(config, "[core] excludesFile = /outside/evil")
            .expect("inline external ignore override writes");

        let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
            .expect_err("inline external ignore file rejects");

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
    fn oversized_pack_file_is_rejected_before_object_database_attachment() {
        let fixture = Fixture::new();
        plant_sparse_pack(
            fixture.root(),
            "oversized.pack",
            (MAX_PACK_FILE_BYTES + 1) as u64,
        );

        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Status)
            .expect_err("oversized captured pack rejects");

        assert_eq!(failure, LocalGitFailure::Repository);
    }

    #[test]
    fn aggregate_object_database_bytes_are_rejected_before_attachment() {
        let fixture = Fixture::new();
        plant_sparse_pack(
            fixture.root(),
            "aggregate-a.pack",
            (MAX_OBJECT_DATABASE_BYTES / 2) as u64,
        );
        plant_sparse_pack(
            fixture.root(),
            "aggregate-b.pack",
            (MAX_OBJECT_DATABASE_BYTES / 2) as u64,
        );

        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Status)
            .expect_err("aggregate captured object bytes reject");

        assert_eq!(failure, LocalGitFailure::Repository);
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
    fn stage_argument_rejects_the_repository_administration_directory() {
        let arguments = NormalizedToolArguments::try_from_provider_text(
            serde_json::json!({"paths": [".git/config"]}).to_string(),
        )
        .expect("JSON arguments normalize");

        assert!(decode_operation(LocalToolKind::Stage, &arguments).is_err());
    }

    #[test]
    fn administrative_scan_stays_on_the_pinned_directory_after_path_replacement() {
        let fixture = Fixture::new();
        let git_path = fixture.root().join(".git");
        let retired_git = fixture.root().join(".git.retired");
        let outside = tempfile::tempdir().expect("outside directory constructs");
        let outside_target = tempfile::tempdir().expect("outside symlink target constructs");
        symlink(outside_target.path(), outside.path().join("escape"))
            .expect("outside administrative symlink constructs");
        let git_directory = openat(
            CWD,
            &git_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("fixture administrative directory pins");
        fs::rename(&git_path, &retired_git).expect("fixture administrative directory retires");
        symlink(outside.path(), &git_path).expect("replacement administrative symlink constructs");

        reject_administrative_symlinks(&git_directory)
            .expect("pinned original administrative directory validates");
        fs::remove_file(&git_path).expect("replacement administrative symlink removes");
        fs::rename(retired_git, git_path).expect("fixture administrative directory restores");

        assert!(outside.path().join("escape").is_symlink());
    }

    #[test]
    fn status_does_not_reclassify_a_conflict_stage_as_untracked() {
        let fixture = Fixture::new();
        install_deleted_conflict(&fixture);
        let conflict_worktree_content = "conflict candidate\n";
        fs::write(fixture.root().join(TRACKED_PATH), conflict_worktree_content)
            .expect("conflict worktree file writes");
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);
        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
        let entry = status["entries"]
            .as_array()
            .expect("status entries are an array")
            .iter()
            .find(|entry| entry["path"] == TRACKED_PATH)
            .expect("conflicted path is reported");
        let patch = diff["patch"].as_str().expect("conflict patch is text");

        assert_eq!(entry["index"], "conflicted");
        assert_eq!(entry["worktree"], "unchanged");
        assert!(patch.contains(conflict_worktree_content));
        assert!(!patch.contains("deleted file mode"));
        assert_eq!(
            fs::read(fixture.root().join(TRACKED_PATH)).expect("conflict worktree file reads"),
            conflict_worktree_content.as_bytes()
        );
    }

    #[test]
    fn checkout_error_rolls_back_a_partially_written_worktree() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let current_tree = repository
            .find_commit(fixture.initial)
            .expect("fixture initial commit opens")
            .tree()
            .expect("fixture initial tree opens");
        let checkout_paths = BTreeSet::from([PathBuf::from(TRACKED_PATH)]);
        let checkout_started = Cell::new(false);

        let failure = checkout_tree_with_rollback(
            &repository,
            Some(&current_tree),
            &checkout_paths,
            &checkout_started,
            || {
                checkout_started.set(true);
                fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
                    .expect("partial checkout fixture writes");
                Err(LocalGitFailure::Operation)
            },
        )
        .expect_err("partial checkout error is reported");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            fs::read(fixture.root().join(TRACKED_PATH)).expect("rolled-back fixture content reads"),
            INITIAL_CONTENT.as_bytes()
        );
    }

    #[test]
    fn commit_restores_reflogs_when_reference_publication_fails() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let tree = repository
            .find_commit(fixture.initial)
            .expect("fixture commit opens")
            .tree_id();
        let new = raw_commit_with_tree(&repository, tree, fixture.initial);
        let executor = fixture.executor();
        let (chain, old) = resolve_pinned_reference_chain(&executor.repository_authority, None)
            .expect("fixture reference chain resolves");
        let update_reference = chain.last().expect("fixture branch target exists");
        let update_lock = ReferenceLock::acquire(&executor.repository_authority, update_reference)
            .expect("fixture target reference locks");
        let reference_path = fixture.root().join(".git").join(update_reference);
        let lock_path = reference_path.with_extension("lock");
        let retired_lock = reference_path.with_extension("retired-lock");
        let head_log = fixture.root().join(".git/logs/HEAD");
        let branch_log = fixture.root().join(".git/logs").join(update_reference);
        let original_head_log = fs::read(&head_log).expect("original HEAD reflog reads");
        let original_branch_log = fs::read(&branch_log).expect("original branch reflog reads");
        let replacement_lock = b"replacement reference lock\n";
        let signature = identity()
            .signature()
            .expect("fixture signature constructs");

        let failure = publish_commit_reference_with_hook(
            &executor.repository_authority,
            update_lock,
            update_reference,
            old.expect("fixture parent exists"),
            new,
            &signature,
            || {
                fs::rename(&lock_path, &retired_lock).expect("target reference lock retires");
                fs::write(&lock_path, replacement_lock).expect("replacement lock writes");
            },
        )
        .expect_err("replaced target lock rejects publication");
        fs::remove_file(&lock_path).expect("replacement reference lock removes");
        fs::remove_file(&retired_lock).expect("retired reference lock removes");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            fs::read(&head_log).expect("restored HEAD reflog reads"),
            original_head_log
        );
        assert_eq!(
            fs::read(&branch_log).expect("restored branch reflog reads"),
            original_branch_log
        );
        assert_eq!(
            fs::read_to_string(reference_path).expect("unchanged reference reads"),
            format!("{}\n", fixture.initial)
        );
    }

    #[test]
    fn reflog_rollback_preserves_a_replacement_published_path() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let tree = repository
            .find_commit(fixture.initial)
            .expect("fixture commit opens")
            .tree_id();
        let new = raw_commit_with_tree(&repository, tree, fixture.initial);
        let executor = fixture.executor();
        let mut log = ReferenceLogLock::acquire(&executor.repository_authority, "HEAD")
            .expect("fixture HEAD reflog locks");
        let signature = identity()
            .signature()
            .expect("fixture signature constructs");
        log.append(fixture.initial, new, &signature, "fixture action")
            .expect("fixture reflog appends");
        log.publish().expect("fixture reflog publishes");
        let head_log = fixture.root().join(".git/logs/HEAD");
        let retired_log = fixture.root().join(".git/logs/HEAD.retired");
        let replacement_content = b"replacement reflog remains exact\n";
        fs::rename(&head_log, &retired_log).expect("published reflog retires");
        fs::write(&head_log, replacement_content).expect("replacement reflog writes");

        let failure = log
            .rollback()
            .expect_err("replacement published reflog rejects rollback");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            fs::read(head_log).expect("replacement reflog reads"),
            replacement_content
        );
        assert!(retired_log.exists());
    }

    #[test]
    fn packed_object_install_uses_the_pack_directory_shared_mode() {
        let source = tempfile::tempdir().expect("fixture source directory constructs");
        let destination = tempfile::tempdir().expect("fixture pack directory constructs");
        let directory_mode = 0o2770;
        let expected_file_mode = (directory_mode & 0o666) | 0o600;
        let name = "pack-shared-mode.pack";
        let source_path = source.path().join(name);
        let destination_path = destination.path().join(name);
        fs::write(&source_path, b"shared pack fixture").expect("fixture source pack writes");
        fs::set_permissions(
            destination.path(),
            fs::Permissions::from_mode(directory_mode),
        )
        .expect("fixture pack directory permissions set");
        let directory = openat(
            CWD,
            destination.path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("fixture pack directory pins");
        let mode = pack_installation_mode(&directory).expect("fixture pack mode resolves");

        install_packed_object_file(&directory, &source_path, mode).expect("fixture pack installs");
        let installed_mode = fs::metadata(destination_path)
            .expect("installed pack metadata reads")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(installed_mode, expected_file_mode);
    }

    #[test]
    fn packed_object_install_removes_its_lock_after_copy_failure() {
        let source = tempfile::tempdir().expect("fixture source directory constructs");
        let destination = tempfile::tempdir().expect("fixture pack directory constructs");
        let name = "pack-copy-failure.pack";
        let lock_name = format!("{name}.lock");
        let source_path = source.path().join(name);
        let destination_path = destination.path().join(name);
        let lock_path = destination.path().join(lock_name);
        let source_content = b"trusted pack fixture";
        fs::write(&source_path, source_content).expect("fixture source pack writes");
        let directory = openat(
            CWD,
            destination.path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("fixture pack directory pins");
        let mode = pack_installation_mode(&directory).expect("fixture pack mode resolves");

        let failure = install_packed_object_file_with_copy_and_hook(
            &directory,
            &source_path,
            mode,
            |_source, destination| {
                destination.write_all(b"partial pack lock")?;
                Err(std::io::Error::other("fixture copy failure"))
            },
            || {},
        )
        .expect_err("failed pack copy rejects publication");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert!(!lock_path.exists());
        assert!(!destination_path.exists());
        assert_eq!(
            fs::read(source_path).expect("fixture source pack reads"),
            source_content
        );
    }

    #[test]
    fn packed_object_install_rejects_a_replaced_source_lock() {
        let source = tempfile::tempdir().expect("fixture source directory constructs");
        let destination = tempfile::tempdir().expect("fixture pack directory constructs");
        let name = "pack-replaced-lock.pack";
        let lock_name = format!("{name}.lock");
        let source_path = source.path().join(name);
        let destination_path = destination.path().join(name);
        let replacement_lock_path = destination.path().join(&lock_name);
        let source_content = b"trusted pack fixture";
        let replacement_content = b"replacement pack lock";
        fs::write(&source_path, source_content).expect("fixture source pack writes");
        let directory = openat(
            CWD,
            destination.path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("fixture pack directory pins");
        let mode = pack_installation_mode(&directory).expect("fixture pack mode resolves");

        let failure = install_packed_object_file_with_hook(&directory, &source_path, mode, || {
            unlinkat(&directory, &lock_name, AtFlags::empty())
                .expect("fixture source lock unlinks");
            fs::write(&replacement_lock_path, replacement_content)
                .expect("fixture replacement lock writes");
        })
        .expect_err("replaced source lock rejects publication");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert!(!destination_path.exists());
        assert_eq!(
            fs::read(replacement_lock_path).expect("replacement lock reads"),
            replacement_content
        );
        assert_eq!(
            fs::read(source_path).expect("source pack reads"),
            source_content
        );
    }

    #[test]
    fn log_bounds_the_extra_commit_used_only_for_truncation() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let oversized = oversized_commit_object(&repository, fixture.initial);
        let tree = repository
            .find_commit(fixture.initial)
            .expect("fixture initial commit opens")
            .tree_id();
        let raw_commit = format!(
            "tree {tree}\nparent {oversized}\nauthor Signalbox <fixer@example.test> 0 +0000\ncommitter Signalbox <fixer@example.test> 0 +0000\n\nsmall child\n"
        );
        let child = repository
            .odb()
            .expect("fixture object database opens")
            .write(ObjectType::Commit, raw_commit.as_bytes())
            .expect("small child commit writes");
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Log(GitLogArguments {
                revision: child.to_string(),
                max_entries: 1,
            }))
            .expect_err("oversized truncation candidate rejects before parsing");

        assert_eq!(failure, LocalGitFailure::Operation);
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
