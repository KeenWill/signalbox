//! `write_file`, `edit_file`, and `apply_patch`: the workspace mutation tool
//! catalog (`WorkspaceMutationTools`), plus the snapshot-then-commit machinery
//! that stages each proposed change and rejects it before writing if the
//! on-disk file no longer matches the executor's captured snapshot.

use std::{
    collections::BTreeMap,
    error::Error,
    ffi::{OsStr, OsString},
    fmt,
    fs::File,
    io::{Read, Write},
    os::fd::OwnedFd,
    path::{Component, Path},
    sync::atomic::{AtomicU64, Ordering},
};

use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, RenameFlags, Stat, fchmod, fstat, openat, renameat_with,
    statat, unlinkat,
};

use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolArgumentValidator, ToolExecutionInvocation, ToolExecutor,
    ToolExecutorEvidence,
};
use signalbox_domain::{
    BlobDigest, NormalizedToolArguments, ToolEffectClass, ToolExecutionErrorDetail,
    ToolPermissionDefault,
};
use signalbox_tool_contract::{
    ToolContract, ToolContractCompileError, compile_contract_definition,
};

use crate::patch::overlapping_match_starts;
use crate::{
    LocalWorkspaceFileSystem, PatchApplyError, PatchParseError, PatchParseErrorKind,
    PlannedPatchOperation, WorkspaceFileSystem, WorkspacePatch, WorkspacePathRejection,
    WorkspaceResolveError, WorkspaceRoot, parse_patch, plan_patch,
};

pub const WRITE_FILE_NAME: &str = "write_file";
pub const EDIT_FILE_NAME: &str = "edit_file";
pub const APPLY_PATCH_NAME: &str = "apply_patch";

/// Stable mutation-family registry names in declaration order.
pub const WORKSPACE_MUTATION_TOOL_NAMES: [&str; 3] =
    [APPLY_PATCH_NAME, EDIT_FILE_NAME, WRITE_FILE_NAME];

/// Maximum UTF-8 byte length admitted for one complete file value.
pub const MAX_WORKSPACE_MUTATION_FILE_BYTES: usize = 1024 * 1024;

const INVALID_ARGUMENTS_DETAIL: &str = "invalid bounded workspace-mutation arguments";
const SNAPSHOT_FAILED_DETAIL: &str = "workspace mutation snapshot failed";
const EDIT_MATCH_FAILED_DETAIL: &str = "workspace edit match requirement failed";
const PATCH_FAILED_DETAIL: &str = "workspace patch prevalidation failed";
const COMMIT_FAILED_DETAIL: &str = "workspace mutation commit failed";

/// Typed `write_file` arguments.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteFileArguments {
    /// Relative destination path inside the injected root.
    #[schemars(length(min = 1, max = crate::path::MAX_WORKSPACE_PATH_CHARACTERS))]
    pub path: String,
    /// Complete UTF-8 replacement content, bounded to one MiB as encoded.
    #[schemars(length(max = MAX_WORKSPACE_MUTATION_FILE_BYTES))]
    pub content: String,
}

struct WriteFileContract;

impl ToolContract for WriteFileContract {
    type Arguments = WriteFileArguments;
    const NAME: &'static str = WRITE_FILE_NAME;
    const DESCRIPTION: &'static str =
        "Creates or overwrites one bounded UTF-8 file inside the injected workspace root.";
}

/// Typed `edit_file` arguments.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditFileArguments {
    /// Relative existing file path inside the injected root.
    #[schemars(length(min = 1, max = crate::path::MAX_WORKSPACE_PATH_CHARACTERS))]
    pub path: String,
    /// Exact non-empty source text to replace.
    #[schemars(length(min = 1, max = MAX_WORKSPACE_MUTATION_FILE_BYTES))]
    pub old_string: String,
    /// Replacement text.
    #[schemars(length(max = MAX_WORKSPACE_MUTATION_FILE_BYTES))]
    pub new_string: String,
    /// Replaces every occurrence instead of requiring one unique occurrence.
    #[serde(default)]
    pub replace_all: bool,
}

struct EditFileContract;

impl ToolContract for EditFileContract {
    type Arguments = EditFileArguments;
    const NAME: &'static str = EDIT_FILE_NAME;
    const DESCRIPTION: &'static str =
        "Replaces unique text in one workspace file; replacement of all matches is explicit.";
}

/// Typed `apply_patch` arguments.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyPatchArguments {
    /// Codex-style patch: start with `*** Begin Patch`; use `*** Add File: path`
    /// followed by `+` lines, `*** Update File: path` with `@@` hunks and
    /// space/`-`/`+` lines, or `*** Delete File: path`; finish with
    /// `*** End Patch`. Empty adds and no-final-newline edits are unsupported.
    #[schemars(length(min = 1, max = crate::MAX_PATCH_BYTES))]
    pub patch: String,
}

struct ApplyPatchContract;

impl ToolContract for ApplyPatchContract {
    type Arguments = ApplyPatchArguments;
    const NAME: &'static str = APPLY_PATCH_NAME;
    const DESCRIPTION: &'static str = concat!(
        "Applies a prevalidated Codex-style patch: `*** Begin Patch`; ",
        "`*** Add File: path` plus `+` lines, `*** Update File: path` plus `@@` hunks whose ",
        "lines start with space, `-`, or `+`, or `*** Delete File: path`; then ",
        "`*** End Patch`. Empty adds and no-final-newline edits are unsupported."
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MutationToolKind {
    ApplyPatch,
    EditFile,
    WriteFile,
}

impl MutationToolKind {
    const ALL: [Self; 3] = [Self::ApplyPatch, Self::EditFile, Self::WriteFile];

    fn definition(self) -> Result<signalbox_application::ToolDefinition, ToolContractCompileError> {
        match self {
            Self::ApplyPatch => compile_contract_definition::<ApplyPatchContract>(
                ToolPermissionDefault::Confirm,
                ToolEffectClass::ExternalEffect,
            ),
            Self::EditFile => compile_contract_definition::<EditFileContract>(
                ToolPermissionDefault::Confirm,
                ToolEffectClass::ExternalEffect,
            ),
            Self::WriteFile => compile_contract_definition::<WriteFileContract>(
                ToolPermissionDefault::Confirm,
                ToolEffectClass::ExternalEffect,
            ),
        }
    }
}

/// One normalized relative workspace mutation path.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceMutationPath(String);

impl WorkspaceMutationPath {
    /// Validates one bounded relative path without touching the filesystem.
    pub fn try_new(supplied: impl Into<String>) -> Result<Self, WorkspacePathRejection> {
        let supplied = supplied.into();
        crate::path::validate_relative_path(&supplied)?;
        Ok(Self(supplied))
    }

    /// Returns normalized model-supplied path text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One file value captured before a workspace mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceFileSnapshot {
    /// Relative workspace path.
    pub path: WorkspaceMutationPath,
    /// Complete UTF-8 content, or `None` when the path did not exist.
    pub content: Option<String>,
    /// Raw mode bits captured for an existing regular file.
    pub mode: Option<u32>,
}

/// A complete precondition snapshot for all paths touched by one operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceMutationSnapshot {
    files: BTreeMap<WorkspaceMutationPath, Option<String>>,
    modes: BTreeMap<WorkspaceMutationPath, Option<u32>>,
}

impl WorkspaceMutationSnapshot {
    /// Builds a snapshot while rejecting duplicate path evidence.
    pub fn try_new(
        files: impl IntoIterator<Item = WorkspaceFileSnapshot>,
    ) -> Result<Self, WorkspaceMutationSnapshotError> {
        let mut collected = BTreeMap::new();
        let mut modes = BTreeMap::new();
        for file in files {
            match collected.entry(file.path) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    modes.insert(entry.key().clone(), file.mode);
                    entry.insert(file.content);
                }
                std::collections::btree_map::Entry::Occupied(entry) => {
                    return Err(WorkspaceMutationSnapshotError {
                        path: Some(entry.key().clone()),
                        kind: WorkspaceMutationSnapshotErrorKind::DuplicatePath,
                    });
                }
            }
        }
        Ok(Self {
            files: collected,
            modes,
        })
    }

    /// Returns captured content for one requested path.
    pub fn content(&self, path: &WorkspaceMutationPath) -> Option<&Option<String>> {
        self.files.get(path)
    }

    /// Returns captured raw mode bits for one requested path.
    pub fn mode(&self, path: &WorkspaceMutationPath) -> Option<&Option<u32>> {
        self.modes.get(path)
    }

    /// Iterates captured paths in lexical order without cloning file content.
    pub fn paths(&self) -> impl Iterator<Item = &WorkspaceMutationPath> {
        self.files.keys()
    }

    /// Iterates captured files in lexical path order.
    pub fn files(&self) -> impl Iterator<Item = WorkspaceFileSnapshot> + '_ {
        self.files
            .iter()
            .map(|(path, content)| WorkspaceFileSnapshot {
                path: path.clone(),
                content: content.clone(),
                mode: self.modes.get(path).copied().flatten(),
            })
    }
}

/// Why a filesystem adapter could not capture a mutation snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceMutationSnapshotErrorKind {
    /// A path escaped or violated the injected-root boundary.
    PathRejected(WorkspacePathRejection),
    /// An existing target was not a regular file.
    NotRegularFile,
    /// Existing content was not UTF-8.
    NotUtf8,
    /// Existing content exceeded the requested byte bound.
    TooLarge,
    /// The same path appeared twice in adapter evidence.
    DuplicatePath,
    /// The filesystem operation failed.
    Filesystem,
}

/// Typed snapshot failure with the affected path when one is known.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceMutationSnapshotError {
    /// Affected relative path.
    pub path: Option<WorkspaceMutationPath>,
    /// Typed failure reason.
    pub kind: WorkspaceMutationSnapshotErrorKind,
}

impl fmt::Display for WorkspaceMutationSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(path) = &self.path {
            write!(
                formatter,
                "workspace snapshot failed for {:?}: ",
                path.as_str()
            )?;
        } else {
            formatter.write_str("workspace snapshot failed: ")?;
        }
        write!(formatter, "{:?}", self.kind)
    }
}

impl Error for WorkspaceMutationSnapshotError {}

/// One fully planned file mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceFileMutation {
    /// Creates or overwrites a complete file value.
    Write {
        /// Relative workspace path.
        path: WorkspaceMutationPath,
        /// Complete UTF-8 content.
        content: String,
    },
    /// Deletes an existing file.
    Delete {
        /// Relative workspace path.
        path: WorkspaceMutationPath,
    },
}

impl WorkspaceFileMutation {
    /// Returns the path touched by this mutation.
    pub fn path(&self) -> &WorkspaceMutationPath {
        match self {
            Self::Write { path, .. } | Self::Delete { path } => path,
        }
    }
}

#[derive(signalbox_derive::OperatorError)]
/// Why an adapter could not commit a prevalidated mutation batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceMutationCommitError {
    #[error("workspace changed before commit")]
    /// A captured precondition changed before commit.
    Conflict,
    #[error("workspace path {:?} rejected: {reason}", path.as_str())]
    /// A path escaped or violated the injected-root boundary.
    PathRejected {
        /// Affected relative path.
        path: WorkspaceMutationPath,
        /// Typed rejection evidence.
        reason: WorkspacePathRejection,
    },
    #[error("workspace commit failed")]
    /// The filesystem failed before any target effect was observed.
    Filesystem,
    #[error("workspace commit outcome is ambiguous")]
    /// A post-effect failure made the final workspace state unknowable.
    Ambiguous,
}

/// Injected filesystem authority for workspace mutation tools.
///
/// Implementations must resolve every `path` relative to `root` without
/// reopening an ambient absolute pathname. `snapshot` must reject symlinks and
/// non-regular files. `commit_atomically` rejects changed preconditions before
/// installing files sequentially, with in-process rollback on failure. Concurrent
/// observers and interrupted processes can see a partial batch; a failed rollback
/// reports an ambiguous outcome.
pub trait WorkspaceMutationFileSystem: Clone + Send + Sync + 'static {
    /// Pinned root authority retained by the executor.
    type Root: Clone + Send + Sync + 'static;

    /// Opens and pins one injected workspace root.
    fn open_root(&self, root: &Path) -> Result<Self::Root, WorkspaceMutationSnapshotError>;

    /// Captures complete bounded values for every requested path.
    fn snapshot(
        &self,
        root: &Self::Root,
        paths: &[WorkspaceMutationPath],
        max_file_bytes: usize,
    ) -> Result<WorkspaceMutationSnapshot, WorkspaceMutationSnapshotError>;

    /// Commits a prevalidated batch against its captured values.
    /// A known failure must leave every target unchanged; an adapter that
    /// cannot prove rollback must return `WorkspaceMutationCommitError::Ambiguous`.
    fn commit_atomically(
        &self,
        root: &Self::Root,
        expected: &WorkspaceMutationSnapshot,
        mutations: &[WorkspaceFileMutation],
    ) -> Result<(), WorkspaceMutationCommitError>;
}

#[derive(signalbox_derive::OperatorError)]
/// A static mutation declaration could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceMutationToolConstructionError {
    #[error("workspace mutation-tool static name is invalid")]
    /// One static contract name was invalid.
    Name,
    #[error("workspace mutation-tool static schema is invalid")]
    /// One static contract schema was invalid.
    Schema,
    #[error("workspace mutation-tool static detail is invalid")]
    /// One static error detail was invalid.
    ErrorDetail,
    #[error("workspace mutation-tool catalog is duplicated")]
    /// The catalog unexpectedly contained a duplicate.
    Duplicate,
    #[error("workspace mutation-tool root is invalid")]
    /// The injected root could not be pinned.
    Root,
}

/// Compiled mutation catalog and executor around one injected root authority.
pub struct WorkspaceMutationTools<FileSystem: WorkspaceMutationFileSystem> {
    catalog: CompiledToolCatalog,
    executor: WorkspaceMutationExecutor<FileSystem>,
}

impl<FileSystem: WorkspaceMutationFileSystem> WorkspaceMutationTools<FileSystem> {
    /// Compiles the three mutation tools around an injected filesystem/root.
    pub fn try_new(
        filesystem: FileSystem,
        root: impl AsRef<Path>,
    ) -> Result<Self, WorkspaceMutationToolConstructionError> {
        let root = filesystem
            .open_root(root.as_ref())
            .map_err(|_| WorkspaceMutationToolConstructionError::Root)?;
        let invalid_arguments_detail = detail(INVALID_ARGUMENTS_DETAIL)?;
        let snapshot_failed_detail = detail(SNAPSHOT_FAILED_DETAIL)?;
        let edit_match_failed_detail = detail(EDIT_MATCH_FAILED_DETAIL)?;
        let patch_failed_detail = detail(PATCH_FAILED_DETAIL)?;
        let commit_failed_detail = detail(COMMIT_FAILED_DETAIL)?;
        let compiled = MutationToolKind::ALL
            .into_iter()
            .map(|kind| {
                let definition = kind.definition().map_err(|error| match error {
                    ToolContractCompileError::Name => WorkspaceMutationToolConstructionError::Name,
                    ToolContractCompileError::Schema => {
                        WorkspaceMutationToolConstructionError::Schema
                    }
                })?;
                Ok(CompiledTool::new(
                    definition,
                    WorkspaceMutationArgumentValidator {
                        kind,
                        detail: invalid_arguments_detail.clone(),
                    },
                ))
            })
            .collect::<Result<Vec<_>, WorkspaceMutationToolConstructionError>>()?;
        let catalog = CompiledToolCatalog::try_new(compiled)
            .map_err(|_| WorkspaceMutationToolConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: WorkspaceMutationExecutor {
                filesystem,
                root,
                snapshot_failed_detail,
                edit_match_failed_detail,
                patch_failed_detail,
                commit_failed_detail,
            },
        })
    }

    /// Returns the catalog and executor as separate composition roles.
    pub fn into_parts(self) -> (CompiledToolCatalog, WorkspaceMutationExecutor<FileSystem>) {
        (self.catalog, self.executor)
    }
}

fn detail(value: &str) -> Result<ToolExecutionErrorDetail, WorkspaceMutationToolConstructionError> {
    ToolExecutionErrorDetail::try_new(String::from(value))
        .map_err(|_| WorkspaceMutationToolConstructionError::ErrorDetail)
}

#[derive(Clone, Debug)]
struct WorkspaceMutationArgumentValidator {
    kind: MutationToolKind,
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for WorkspaceMutationArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_operation(self.kind, arguments)
            .map(|_| ())
            .map_err(|error| error.tool_detail().unwrap_or_else(|| self.detail.clone()))
    }
}

#[derive(Debug)]
enum InvalidMutationArguments {
    Shape,
    TooLarge {
        field: &'static str,
        actual_bytes: usize,
        maximum_bytes: usize,
        bound: &'static str,
    },
    Path(WorkspacePathRejection),
    Patch(PatchParseError),
}

impl InvalidMutationArguments {
    fn tool_detail(&self) -> Option<ToolExecutionErrorDetail> {
        let detail = match self {
            Self::Shape => return None,
            Self::TooLarge {
                field,
                actual_bytes,
                maximum_bytes,
                bound,
            } => format!(
                "workspace mutation argument {field:?} has {actual_bytes} UTF-8 bytes; \
                 {bound} is {maximum_bytes} bytes"
            ),
            Self::Path(reason) => format!("workspace mutation path rejected: {reason}"),
            Self::Patch(error) => patch_parse_detail(error),
        };
        ToolExecutionErrorDetail::try_new(detail).ok()
    }
}

fn patch_parse_detail(error: &PatchParseError) -> String {
    let operation = error
        .location
        .operation
        .map_or_else(String::new, |value| format!(", operation {value}"));
    let hunk = error
        .location
        .hunk
        .map_or_else(String::new, |value| format!(", hunk {value}"));
    let reason = match &error.kind {
        PatchParseErrorKind::PathRejected { reason, .. } => {
            format!("path rejected: {reason:?}")
        }
        kind => format!("{kind:?}"),
    };
    format!(
        "patch parse failed at line {}{operation}{hunk}: {reason}",
        error.location.line
    )
}

enum MutationOperation {
    Write {
        path: WorkspaceMutationPath,
        content: String,
    },
    Edit {
        path: WorkspaceMutationPath,
        old_string: String,
        new_string: String,
        replace_all: bool,
    },
    ApplyPatch(WorkspacePatch),
}

fn validate_argument_byte_length(
    field: &'static str,
    actual_bytes: usize,
    maximum_bytes: usize,
    bound: &'static str,
) -> Result<(), InvalidMutationArguments> {
    if actual_bytes > maximum_bytes {
        return Err(InvalidMutationArguments::TooLarge {
            field,
            actual_bytes,
            maximum_bytes,
            bound,
        });
    }
    Ok(())
}

fn decode_operation(
    kind: MutationToolKind,
    arguments: &NormalizedToolArguments,
) -> Result<MutationOperation, InvalidMutationArguments> {
    match kind {
        MutationToolKind::WriteFile => {
            let decoded: WriteFileArguments = serde_json::from_str(arguments.as_str())
                .map_err(|_| InvalidMutationArguments::Shape)?;
            validate_argument_byte_length(
                "content",
                decoded.content.len(),
                MAX_WORKSPACE_MUTATION_FILE_BYTES,
                "MAX_WORKSPACE_MUTATION_FILE_BYTES",
            )?;
            let path = WorkspaceMutationPath::try_new(decoded.path)
                .map_err(InvalidMutationArguments::Path)?;
            Ok(MutationOperation::Write {
                path,
                content: decoded.content,
            })
        }
        MutationToolKind::EditFile => {
            let decoded: EditFileArguments = serde_json::from_str(arguments.as_str())
                .map_err(|_| InvalidMutationArguments::Shape)?;
            if decoded.old_string.is_empty() {
                return Err(InvalidMutationArguments::Shape);
            }
            validate_argument_byte_length(
                "old_string",
                decoded.old_string.len(),
                MAX_WORKSPACE_MUTATION_FILE_BYTES,
                "MAX_WORKSPACE_MUTATION_FILE_BYTES",
            )?;
            validate_argument_byte_length(
                "new_string",
                decoded.new_string.len(),
                MAX_WORKSPACE_MUTATION_FILE_BYTES,
                "MAX_WORKSPACE_MUTATION_FILE_BYTES",
            )?;
            let path = WorkspaceMutationPath::try_new(decoded.path)
                .map_err(InvalidMutationArguments::Path)?;
            Ok(MutationOperation::Edit {
                path,
                old_string: decoded.old_string,
                new_string: decoded.new_string,
                replace_all: decoded.replace_all,
            })
        }
        MutationToolKind::ApplyPatch => {
            let decoded: ApplyPatchArguments = serde_json::from_str(arguments.as_str())
                .map_err(|_| InvalidMutationArguments::Shape)?;
            validate_argument_byte_length(
                "patch",
                decoded.patch.len(),
                crate::MAX_PATCH_BYTES,
                "MAX_PATCH_BYTES",
            )?;
            parse_patch(&decoded.patch)
                .map(MutationOperation::ApplyPatch)
                .map_err(InvalidMutationArguments::Patch)
        }
    }
}

fn replacement_output_bytes(
    source_bytes: usize,
    old_bytes: usize,
    new_bytes: usize,
    matches: usize,
) -> Option<usize> {
    let removed = old_bytes.checked_mul(matches)?;
    let retained = source_bytes.checked_sub(removed)?;
    let added = new_bytes.checked_mul(matches)?;
    retained.checked_add(added)
}

/// Executor for the three approved workspace mutation tools.
pub struct WorkspaceMutationExecutor<FileSystem: WorkspaceMutationFileSystem> {
    filesystem: FileSystem,
    root: FileSystem::Root,
    snapshot_failed_detail: ToolExecutionErrorDetail,
    edit_match_failed_detail: ToolExecutionErrorDetail,
    patch_failed_detail: ToolExecutionErrorDetail,
    commit_failed_detail: ToolExecutionErrorDetail,
}

#[derive(signalbox_derive::OperatorError)]
/// A checked catalog/executor assumption failed inside the mutation family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceMutationExecutorError {
    #[error("workspace mutation argument validation drifted")]
    /// Executor argument decoding disagreed with catalog validation.
    ArgumentValidationDrift,
    #[error("workspace mutation result encoding failed")]
    /// Compact result encoding unexpectedly failed.
    ResultEncoding,
}

impl ClassifyOperatorFailure for WorkspaceMutationExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::CallerOrHubBug
    }
}

impl<FileSystem: WorkspaceMutationFileSystem> ToolExecutor
    for WorkspaceMutationExecutor<FileSystem>
{
    type Error = WorkspaceMutationExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let kind = kind_for_name(invocation.request().name().as_str())
            .ok_or(WorkspaceMutationExecutorError::ArgumentValidationDrift)?;
        let operation = decode_operation(kind, invocation.request().arguments())
            .map_err(|_| WorkspaceMutationExecutorError::ArgumentValidationDrift)?;
        let evidence = match self.execute_operation(operation) {
            Ok(result) => ToolExecutorEvidence::CompletedText(
                serde_json::to_string(&result)
                    .map_err(|_| WorkspaceMutationExecutorError::ResultEncoding)?,
            ),
            Err(MutationFailure::Snapshot) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.snapshot_failed_detail.clone()),
            },
            Err(MutationFailure::EditMatch) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.edit_match_failed_detail.clone()),
            },
            Err(MutationFailure::Patch(detail)) => ToolExecutorEvidence::KnownFailed {
                detail: Some(
                    ToolExecutionErrorDetail::try_new(detail)
                        .unwrap_or_else(|_| self.patch_failed_detail.clone()),
                ),
            },
            Err(MutationFailure::Commit) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.commit_failed_detail.clone()),
            },
            Err(MutationFailure::Ambiguous) => ToolExecutorEvidence::Ambiguous,
        };
        Ok(invocation.bind(evidence))
    }
}

fn kind_for_name(name: &str) -> Option<MutationToolKind> {
    match name {
        APPLY_PATCH_NAME => Some(MutationToolKind::ApplyPatch),
        EDIT_FILE_NAME => Some(MutationToolKind::EditFile),
        WRITE_FILE_NAME => Some(MutationToolKind::WriteFile),
        _ => None,
    }
}

enum MutationResult {
    Write(WriteFileResult),
    Edit(EditFileResult),
    Patch(ApplyPatchResult),
}

impl serde::Serialize for MutationResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Write(result) => result.serialize(serializer),
            Self::Edit(result) => result.serialize(serializer),
            Self::Patch(result) => result.serialize(serializer),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum MutationFailure {
    Snapshot,
    EditMatch,
    Patch(String),
    Commit,
    Ambiguous,
}

impl<FileSystem: WorkspaceMutationFileSystem> WorkspaceMutationExecutor<FileSystem> {
    fn execute_operation(
        &self,
        operation: MutationOperation,
    ) -> Result<MutationResult, MutationFailure> {
        match operation {
            MutationOperation::Write { path, content } => {
                self.write_file(path, content).map(MutationResult::Write)
            }
            MutationOperation::Edit {
                path,
                old_string,
                new_string,
                replace_all,
            } => self
                .edit_file(path, &old_string, &new_string, replace_all)
                .map(MutationResult::Edit),
            MutationOperation::ApplyPatch(patch) => {
                self.apply_patch(&patch).map(MutationResult::Patch)
            }
        }
    }

    fn capture(
        &self,
        paths: &[WorkspaceMutationPath],
    ) -> Result<WorkspaceMutationSnapshot, MutationFailure> {
        self.filesystem
            .snapshot(&self.root, paths, MAX_WORKSPACE_MUTATION_FILE_BYTES)
            .map_err(|_| MutationFailure::Snapshot)
    }

    fn commit(
        &self,
        snapshot: &WorkspaceMutationSnapshot,
        mutations: &[WorkspaceFileMutation],
    ) -> Result<(), MutationFailure> {
        self.filesystem
            .commit_atomically(&self.root, snapshot, mutations)
            .map_err(|error| match error {
                WorkspaceMutationCommitError::Ambiguous => MutationFailure::Ambiguous,
                WorkspaceMutationCommitError::Conflict
                | WorkspaceMutationCommitError::PathRejected { .. }
                | WorkspaceMutationCommitError::Filesystem => MutationFailure::Commit,
            })
    }

    fn write_file(
        &self,
        path: WorkspaceMutationPath,
        content: String,
    ) -> Result<WriteFileResult, MutationFailure> {
        let snapshot = self.capture(std::slice::from_ref(&path))?;
        let created = snapshot.content(&path).is_some_and(Option::is_none);
        let bytes_written = content.len();
        self.commit(
            &snapshot,
            &[WorkspaceFileMutation::Write {
                path: path.clone(),
                content,
            }],
        )?;
        Ok(WriteFileResult {
            path: path.0,
            bytes_written,
            created,
        })
    }

    fn edit_file(
        &self,
        path: WorkspaceMutationPath,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> Result<EditFileResult, MutationFailure> {
        let snapshot = self.capture(std::slice::from_ref(&path))?;
        let source = snapshot
            .content(&path)
            .and_then(Option::as_deref)
            .ok_or(MutationFailure::EditMatch)?;
        let matches = if replace_all {
            source.match_indices(old_string).count()
        } else {
            overlapping_match_starts(source, old_string).len()
        };
        if matches == 0 || !replace_all && matches != 1 {
            return Err(MutationFailure::EditMatch);
        }
        let output_bytes =
            replacement_output_bytes(source.len(), old_string.len(), new_string.len(), matches)
                .filter(|bytes| *bytes <= MAX_WORKSPACE_MUTATION_FILE_BYTES)
                .ok_or(MutationFailure::EditMatch)?;
        let content = if replace_all {
            source.replace(old_string, new_string)
        } else {
            source.replacen(old_string, new_string, 1)
        };
        debug_assert_eq!(content.len(), output_bytes);
        let bytes_written = content.len();
        self.commit(
            &snapshot,
            &[WorkspaceFileMutation::Write {
                path: path.clone(),
                content,
            }],
        )?;
        Ok(EditFileResult {
            path: path.0,
            replacements: matches,
            bytes_written,
        })
    }

    fn apply_patch(&self, patch: &WorkspacePatch) -> Result<ApplyPatchResult, MutationFailure> {
        let paths = patch
            .operations()
            .iter()
            .map(|operation| WorkspaceMutationPath::try_new(String::from(operation.path())))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| generic_patch_failure())?;
        let snapshot = self.capture(&paths)?;
        let contents = snapshot
            .files()
            .filter_map(|file| file.content.map(|content| (file.path.0, content)))
            .collect::<BTreeMap<_, _>>();
        let plan = plan_patch(patch, &contents).map_err(patch_apply_failure)?;
        let mutations = plan
            .operations()
            .iter()
            .map(planned_mutation)
            .collect::<Result<Vec<_>, _>>()?;
        self.commit(&snapshot, &mutations)?;
        Ok(ApplyPatchResult {
            operations_applied: mutations.len(),
        })
    }
}

fn planned_mutation(
    operation: &PlannedPatchOperation,
) -> Result<WorkspaceFileMutation, MutationFailure> {
    match operation {
        PlannedPatchOperation::Add { path, content }
        | PlannedPatchOperation::Update { path, content } => {
            if content.len() > MAX_WORKSPACE_MUTATION_FILE_BYTES {
                return Err(generic_patch_failure());
            }
            Ok(WorkspaceFileMutation::Write {
                path: WorkspaceMutationPath::try_new(path.clone())
                    .map_err(|_| generic_patch_failure())?,
                content: content.clone(),
            })
        }
        PlannedPatchOperation::Delete { path } => Ok(WorkspaceFileMutation::Delete {
            path: WorkspaceMutationPath::try_new(path.clone())
                .map_err(|_| generic_patch_failure())?,
        }),
    }
}

fn generic_patch_failure() -> MutationFailure {
    MutationFailure::Patch(String::from(PATCH_FAILED_DETAIL))
}

fn patch_apply_failure(error: PatchApplyError) -> MutationFailure {
    let hunk = error.hunk.map_or_else(
        || String::from("file operation"),
        |hunk| format!("hunk {hunk}"),
    );
    let path = bounded_detail_path(&error.path);
    MutationFailure::Patch(format!(
        "patch operation {}, path {path:?}, {hunk} failed: {:?}",
        error.operation, error.kind
    ))
}

fn bounded_detail_path(path: &str) -> &str {
    const MAX_DETAIL_PATH_BYTES: usize = 1024;

    let mut end = path.len().min(MAX_DETAIL_PATH_BYTES);
    while !path.is_char_boundary(end) {
        end -= 1;
    }
    &path[..end]
}

/// Successful `write_file` result.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct WriteFileResult {
    /// Relative destination path.
    pub path: String,
    /// UTF-8 bytes written.
    pub bytes_written: usize,
    /// Whether the destination did not previously exist.
    pub created: bool,
}

/// Successful `edit_file` result.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct EditFileResult {
    /// Relative edited path.
    pub path: String,
    /// Number of replacements made.
    pub replacements: usize,
    /// Complete UTF-8 bytes written.
    pub bytes_written: usize,
}

/// Successful `apply_patch` result.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ApplyPatchResult {
    /// Number of file operations committed.
    pub operations_applied: usize,
}

impl WorkspaceMutationFileSystem for LocalWorkspaceFileSystem {
    type Root = WorkspaceRoot;

    fn open_root(&self, root: &Path) -> Result<Self::Root, WorkspaceMutationSnapshotError> {
        WorkspaceRoot::try_new(self, root).map_err(|_| WorkspaceMutationSnapshotError {
            path: None,
            kind: WorkspaceMutationSnapshotErrorKind::Filesystem,
        })
    }

    fn snapshot(
        &self,
        root: &Self::Root,
        paths: &[WorkspaceMutationPath],
        max_file_bytes: usize,
    ) -> Result<WorkspaceMutationSnapshot, WorkspaceMutationSnapshotError> {
        let files = paths
            .iter()
            .map(|path| local_file_snapshot(self, root, path, max_file_bytes))
            .collect::<Result<Vec<_>, _>>()?;
        WorkspaceMutationSnapshot::try_new(files)
    }

    fn commit_atomically(
        &self,
        root: &Self::Root,
        expected: &WorkspaceMutationSnapshot,
        mutations: &[WorkspaceFileMutation],
    ) -> Result<(), WorkspaceMutationCommitError> {
        let paths = expected.paths().cloned().collect::<Vec<_>>();
        let current = self
            .snapshot(root, &paths, MAX_WORKSPACE_MUTATION_FILE_BYTES)
            .map_err(snapshot_commit_error)?;
        if current != *expected {
            return Err(WorkspaceMutationCommitError::Conflict);
        }

        let mut staged = Vec::with_capacity(mutations.len());
        for mutation in mutations {
            match stage_mutation(root, expected, mutation) {
                Ok(file) => staged.push(file),
                Err(error) => {
                    return match cleanup_staged(&mut staged) {
                        Ok(()) => Err(error),
                        Err(()) => Err(WorkspaceMutationCommitError::Ambiguous),
                    };
                }
            }
        }

        for index in 0..staged.len() {
            let verified = match verify_precondition(root, expected, &staged[index]) {
                Ok(verified) => verified,
                Err(error) => return Err(rollback_result(&mut staged, error)),
            };
            if let Err(error) =
                install_staged(root, &mut staged[index], expected, verified.as_ref())
            {
                return Err(rollback_result(&mut staged, error));
            }
        }
        cleanup_backups(&mut staged).map_err(|()| WorkspaceMutationCommitError::Ambiguous)
    }
}

fn verify_precondition(
    root: &WorkspaceRoot,
    expected: &WorkspaceMutationSnapshot,
    staged: &StagedMutation,
) -> Result<Option<File>, WorkspaceMutationCommitError> {
    let parent = revalidate_mutation_parent(root, staged)?;
    if staged.backup.is_none() {
        // Creation uses NOREPLACE at installation.
        return Ok(None);
    }
    verified_file(expected, staged, &parent, &staged.target).map(Some)
}

fn verified_file(
    expected: &WorkspaceMutationSnapshot,
    staged: &StagedMutation,
    parent: &OwnedFd,
    name: &OsStr,
) -> Result<File, WorkspaceMutationCommitError> {
    let descriptor = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| precondition_errno(&staged.path, error))?;
    let status = fstat(&descriptor).map_err(|error| commit_errno(&staged.path, error))?;
    if FileType::from_raw_mode(status.st_mode) != FileType::RegularFile {
        return Err(WorkspaceMutationCommitError::Conflict);
    }
    let file = File::from(descriptor);
    let mut bytes = Vec::new();
    (&file)
        .take((MAX_WORKSPACE_MUTATION_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| WorkspaceMutationCommitError::Filesystem)?;
    let content = expected
        .content(&staged.path)
        .and_then(Option::as_ref)
        .ok_or(WorkspaceMutationCommitError::Filesystem)?;
    let mode = expected
        .mode(&staged.path)
        .copied()
        .flatten()
        .ok_or(WorkspaceMutationCommitError::Filesystem)?;
    let current_mode: u32 = status.st_mode as _;
    if bytes != content.as_bytes() || current_mode != mode {
        return Err(WorkspaceMutationCommitError::Conflict);
    }
    Ok(file)
}

fn local_file_snapshot(
    filesystem: &LocalWorkspaceFileSystem,
    root: &WorkspaceRoot,
    path: &WorkspaceMutationPath,
    max_file_bytes: usize,
) -> Result<WorkspaceFileSnapshot, WorkspaceMutationSnapshotError> {
    match filesystem.read_file_prefix(root, Path::new(path.as_str()), max_file_bytes) {
        Ok(read) if read.truncated => Err(WorkspaceMutationSnapshotError {
            path: Some(path.clone()),
            kind: WorkspaceMutationSnapshotErrorKind::TooLarge,
        }),
        Ok(read) => String::from_utf8(read.bytes)
            .map(|content| WorkspaceFileSnapshot {
                path: path.clone(),
                content: Some(content),
                mode: Some(read.mode),
            })
            .map_err(|_| WorkspaceMutationSnapshotError {
                path: Some(path.clone()),
                kind: WorkspaceMutationSnapshotErrorKind::NotUtf8,
            }),
        Err(WorkspaceResolveError::Rejected(reason)) => Err(WorkspaceMutationSnapshotError {
            path: Some(path.clone()),
            kind: WorkspaceMutationSnapshotErrorKind::PathRejected(reason),
        }),
        Err(WorkspaceResolveError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(WorkspaceFileSnapshot {
                path: path.clone(),
                content: None,
                mode: None,
            })
        }
        Err(WorkspaceResolveError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::InvalidData =>
        {
            Err(WorkspaceMutationSnapshotError {
                path: Some(path.clone()),
                kind: WorkspaceMutationSnapshotErrorKind::NotRegularFile,
            })
        }
        Err(WorkspaceResolveError::Io { .. }) => Err(WorkspaceMutationSnapshotError {
            path: Some(path.clone()),
            kind: WorkspaceMutationSnapshotErrorKind::Filesystem,
        }),
    }
}

fn snapshot_commit_error(error: WorkspaceMutationSnapshotError) -> WorkspaceMutationCommitError {
    match (error.path, error.kind) {
        (Some(path), WorkspaceMutationSnapshotErrorKind::PathRejected(reason)) => {
            WorkspaceMutationCommitError::PathRejected { path, reason }
        }
        _ => WorkspaceMutationCommitError::Filesystem,
    }
}

struct StagedMutation {
    parent: OwnedFd,
    path: WorkspaceMutationPath,
    target: OsString,
    stage: Option<OsString>,
    backup: Option<OsString>,
    backup_created: bool,
    backup_file: Option<File>,
    target_installed: bool,
    writes_target: bool,
    installed_file: Option<(File, Stat, BlobDigest)>,
}

fn stage_mutation(
    root: &WorkspaceRoot,
    expected: &WorkspaceMutationSnapshot,
    mutation: &WorkspaceFileMutation,
) -> Result<StagedMutation, WorkspaceMutationCommitError> {
    let path = mutation.path().clone();
    let had_original = expected
        .content(&path)
        .is_some_and(|content| content.is_some());
    let (parent, target) = open_mutation_parent(
        root,
        &path,
        if had_original {
            precondition_errno
        } else {
            commit_errno
        },
    )?;
    let backup = had_original.then(|| transaction_name("backup"));
    let (stage, installed_file) = match mutation {
        WorkspaceFileMutation::Write { content, .. } => {
            let name = transaction_name("stage");
            let mode = expected.mode(&path).copied().flatten().unwrap_or(0o600);
            let file = write_staged_file(&parent, &name, content, mode, &path)?;
            (Some(name), Some(file))
        }
        WorkspaceFileMutation::Delete { .. } => (None, None),
    };
    Ok(StagedMutation {
        parent,
        path,
        target,
        stage,
        backup,
        backup_created: false,
        backup_file: None,
        target_installed: false,
        writes_target: installed_file.is_some(),
        installed_file,
    })
}

fn open_mutation_parent(
    root: &WorkspaceRoot,
    path: &WorkspaceMutationPath,
    map_errno: fn(&WorkspaceMutationPath, rustix::io::Errno) -> WorkspaceMutationCommitError,
) -> Result<(OwnedFd, OsString), WorkspaceMutationCommitError> {
    let supplied = Path::new(path.as_str());
    let target = supplied
        .file_name()
        .filter(|name| *name != OsStr::new("."))
        .ok_or_else(|| WorkspaceMutationCommitError::PathRejected {
            path: path.clone(),
            reason: WorkspacePathRejection::Invalid,
        })?
        .to_owned();
    let mut current = openat(
        root.descriptor(),
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| map_errno(path, error))?;
    if let Some(parent) = supplied.parent() {
        for component in parent.components() {
            let Component::Normal(name) = component else {
                continue;
            };
            current = openat(
                &current,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| map_errno(path, error))?;
        }
    }
    Ok((current, target))
}

fn revalidate_mutation_parent(
    root: &WorkspaceRoot,
    staged: &StagedMutation,
) -> Result<OwnedFd, WorkspaceMutationCommitError> {
    let (parent, _) = open_mutation_parent(root, &staged.path, precondition_errno)?;
    let before = fstat(&staged.parent).map_err(|error| commit_errno(&staged.path, error))?;
    let current = fstat(&parent).map_err(|error| commit_errno(&staged.path, error))?;
    if before.st_dev != current.st_dev || before.st_ino != current.st_ino {
        return Err(WorkspaceMutationCommitError::Conflict);
    }
    Ok(parent)
}

fn write_staged_file(
    parent: &OwnedFd,
    name: &OsStr,
    content: &str,
    mode: u32,
    path: &WorkspaceMutationPath,
) -> Result<(File, Stat, BlobDigest), WorkspaceMutationCommitError> {
    let descriptor = openat(
        parent,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|error| commit_errno(path, error))?;
    let mut file = File::from(descriptor);
    let result = file
        .write_all(content.as_bytes())
        .and_then(|()| {
            fchmod(&file, Mode::from_bits_retain((mode & 0o1777) as _))
                .map_err(std::io::Error::from)
        })
        .and_then(|()| file.sync_all())
        .and_then(|()| fstat(&file).map_err(std::io::Error::from));
    if let Ok(identity) = result {
        return Ok((file, identity, BlobDigest::digest(content.as_bytes())));
    }
    drop(file);
    unlinkat(parent, name, AtFlags::empty())
        .map_err(|_| WorkspaceMutationCommitError::Ambiguous)?;
    Err(WorkspaceMutationCommitError::Filesystem)
}

fn install_staged(
    root: &WorkspaceRoot,
    staged: &mut StagedMutation,
    expected: &WorkspaceMutationSnapshot,
    verified: Option<&File>,
) -> Result<(), WorkspaceMutationCommitError> {
    staged.parent = revalidate_mutation_parent(root, staged)?;
    if let Some(backup) = &staged.backup {
        let verified = verified.ok_or(WorkspaceMutationCommitError::Filesystem)?;
        if !name_matches_file(&staged.parent, &staged.target, verified)
            .map_err(|error| precondition_errno(&staged.path, error))?
        {
            return Err(WorkspaceMutationCommitError::Conflict);
        }
        staged.backup_file = Some(
            verified
                .try_clone()
                .map_err(|_| WorkspaceMutationCommitError::Filesystem)?,
        );
        renameat_with(
            &staged.parent,
            &staged.target,
            &staged.parent,
            backup,
            RenameFlags::NOREPLACE,
        )
        .map_err(|error| precondition_errno(&staged.path, error))?;
        staged.backup_created = true;
        let moved = verified_file(expected, staged, &staged.parent, backup)?;
        let before = fstat(verified).map_err(|error| commit_errno(&staged.path, error))?;
        let after = fstat(&moved).map_err(|error| commit_errno(&staged.path, error))?;
        if before.st_dev != after.st_dev || before.st_ino != after.st_ino {
            return Err(WorkspaceMutationCommitError::Conflict);
        }
    }
    staged.parent = revalidate_mutation_parent(root, staged)?;
    if let Some(stage) = staged.stage.as_ref() {
        renameat_with(
            &staged.parent,
            stage,
            &staged.parent,
            &staged.target,
            RenameFlags::NOREPLACE,
        )
        .map_err(|error| {
            if staged.backup.is_none() && error == rustix::io::Errno::EXIST {
                WorkspaceMutationCommitError::Conflict
            } else {
                commit_errno(&staged.path, error)
            }
        })?;
        staged.stage = None;
    }
    staged.target_installed = true;
    Ok(())
}

fn rollback_result(
    staged: &mut [StagedMutation],
    known_error: WorkspaceMutationCommitError,
) -> WorkspaceMutationCommitError {
    match rollback_staged(staged) {
        Ok(()) => known_error,
        Err(()) => WorkspaceMutationCommitError::Ambiguous,
    }
}

fn rollback_staged(staged: &mut [StagedMutation]) -> Result<(), ()> {
    let mut failed = false;
    for file in staged.iter_mut().rev() {
        let can_restore = (!file.backup_created || backup_is_owned(file))
            && (!file.target_installed
                || !file.writes_target
                || remove_installed_target(file).is_ok());
        failed |= !can_restore;
        if can_restore && file.backup_created {
            failed |= restore_backup(file).is_err();
        }
        if let Some(stage) = file.stage.take() {
            failed |= unlinkat(&file.parent, stage, AtFlags::empty()).is_err();
        }
    }
    if failed { Err(()) } else { Ok(()) }
}

fn name_matches_file(
    parent: &OwnedFd,
    name: &OsStr,
    expected: &File,
) -> Result<bool, rustix::io::Errno> {
    let expected = fstat(expected)?;
    let current = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    Ok(current.st_dev == expected.st_dev && current.st_ino == expected.st_ino)
}

fn backup_is_owned(file: &StagedMutation) -> bool {
    file.backup
        .as_ref()
        .zip(file.backup_file.as_ref())
        .is_some_and(|(name, expected)| {
            name_matches_file(&file.parent, name, expected).unwrap_or(false)
        })
}

fn restore_backup(file: &StagedMutation) -> Result<(), ()> {
    let backup = file.backup.as_ref().ok_or(())?;
    let expected = file.backup_file.as_ref().ok_or(())?;
    let displaced = transaction_name("restore");
    renameat_with(
        &file.parent,
        backup,
        &file.parent,
        &displaced,
        RenameFlags::NOREPLACE,
    )
    .map_err(|_| ())?;
    if !name_matches_file(&file.parent, &displaced, expected).unwrap_or(false) {
        let _ = renameat_with(
            &file.parent,
            &displaced,
            &file.parent,
            backup,
            RenameFlags::NOREPLACE,
        );
        return Err(());
    }
    renameat_with(
        &file.parent,
        &displaced,
        &file.parent,
        &file.target,
        RenameFlags::NOREPLACE,
    )
    .map_err(|_| ())?;
    // A source replacement during restoration must not be reported as a known rollback.
    if !name_matches_file(&file.parent, &file.target, expected).unwrap_or(false) {
        return Err(());
    }
    Ok(())
}

fn remove_installed_target(file: &StagedMutation) -> Result<(), ()> {
    if !rollback_target_is_owned(file, &file.target)? {
        return Err(());
    }
    let displaced = transaction_name("rollback");
    renameat_with(
        &file.parent,
        &file.target,
        &file.parent,
        &displaced,
        RenameFlags::NOREPLACE,
    )
    .map_err(|_| ())?;
    let owned = rollback_target_is_owned(file, &displaced).unwrap_or(false);
    if !owned {
        // Never replace a new destination; both displaced and backup files survive on failure.
        let _ = renameat_with(
            &file.parent,
            &displaced,
            &file.parent,
            &file.target,
            RenameFlags::NOREPLACE,
        );
        return Err(());
    }
    unlinkat(&file.parent, &displaced, AtFlags::empty()).map_err(|_| ())
}

fn rollback_target_is_owned(file: &StagedMutation, target: &OsStr) -> Result<bool, ()> {
    let (_installed, expected, digest) = file.installed_file.as_ref().ok_or(())?;
    let descriptor = openat(
        &file.parent,
        target,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| ())?;
    let current = fstat(&descriptor).map_err(|_| ())?;
    if current.st_dev != expected.st_dev
        || current.st_ino != expected.st_ino
        || current.st_size != expected.st_size
        || current.st_mode != expected.st_mode
    {
        return Ok(false);
    }
    let mut bytes = Vec::new();
    File::from(descriptor)
        .take((MAX_WORKSPACE_MUTATION_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    Ok(bytes.len() <= MAX_WORKSPACE_MUTATION_FILE_BYTES && BlobDigest::digest(&bytes) == *digest)
}

fn cleanup_staged(staged: &mut [StagedMutation]) -> Result<(), ()> {
    let mut failed = false;
    for file in staged {
        if let Some(stage) = file.stage.take() {
            failed |= unlinkat(&file.parent, stage, AtFlags::empty()).is_err();
        }
    }
    if failed { Err(()) } else { Ok(()) }
}

fn cleanup_backups(staged: &mut [StagedMutation]) -> Result<(), ()> {
    let mut failed = false;
    for file in staged {
        if let Some(backup) = file.backup.take() {
            failed |= unlinkat(&file.parent, backup, AtFlags::empty()).is_err();
        }
    }
    if failed { Err(()) } else { Ok(()) }
}

fn precondition_errno(
    path: &WorkspaceMutationPath,
    error: rustix::io::Errno,
) -> WorkspaceMutationCommitError {
    if error == rustix::io::Errno::NOENT {
        WorkspaceMutationCommitError::Conflict
    } else {
        commit_errno(path, error)
    }
}

fn commit_errno(
    path: &WorkspaceMutationPath,
    error: rustix::io::Errno,
) -> WorkspaceMutationCommitError {
    if error == rustix::io::Errno::LOOP {
        WorkspaceMutationCommitError::PathRejected {
            path: path.clone(),
            reason: WorkspacePathRejection::Symlink,
        }
    } else {
        WorkspaceMutationCommitError::Filesystem
    }
}

fn transaction_name(role: &str) -> OsString {
    static NEXT_TRANSACTION_FILE: AtomicU64 = AtomicU64::new(1);
    OsString::from(format!(
        ".signalbox-{}-{}-{role}",
        std::process::id(),
        NEXT_TRANSACTION_FILE.fetch_add(1, Ordering::Relaxed)
    ))
}

#[cfg(test)]
mod contract_tests;

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct FakeFileSystem {
        state: Arc<Mutex<FakeState>>,
    }

    #[derive(Default)]
    struct FakeState {
        files: BTreeMap<String, String>,
        fail_commit: bool,
    }

    impl FakeFileSystem {
        fn with_files(files: impl IntoIterator<Item = (&'static str, &'static str)>) -> Self {
            let files = files
                .into_iter()
                .map(|(path, content)| (String::from(path), String::from(content)))
                .collect();
            Self {
                state: Arc::new(Mutex::new(FakeState {
                    files,
                    fail_commit: false,
                })),
            }
        }

        fn files(&self) -> BTreeMap<String, String> {
            self.state
                .lock()
                .expect("fake lock is available")
                .files
                .clone()
        }
    }

    impl WorkspaceMutationFileSystem for FakeFileSystem {
        type Root = std::path::PathBuf;

        fn open_root(&self, root: &Path) -> Result<Self::Root, WorkspaceMutationSnapshotError> {
            Ok(root.to_owned())
        }

        fn snapshot(
            &self,
            _root: &Self::Root,
            paths: &[WorkspaceMutationPath],
            max_file_bytes: usize,
        ) -> Result<WorkspaceMutationSnapshot, WorkspaceMutationSnapshotError> {
            let state = self.state.lock().expect("fake lock is available");
            let files = paths
                .iter()
                .map(|path| {
                    let content = state.files.get(path.as_str()).cloned();
                    if content
                        .as_ref()
                        .is_some_and(|value| value.len() > max_file_bytes)
                    {
                        return Err(WorkspaceMutationSnapshotError {
                            path: Some(path.clone()),
                            kind: WorkspaceMutationSnapshotErrorKind::TooLarge,
                        });
                    }
                    let mode = content.as_ref().map(|_| 0o600);
                    Ok(WorkspaceFileSnapshot {
                        path: path.clone(),
                        content,
                        mode,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            WorkspaceMutationSnapshot::try_new(files)
        }

        fn commit_atomically(
            &self,
            _root: &Self::Root,
            expected: &WorkspaceMutationSnapshot,
            mutations: &[WorkspaceFileMutation],
        ) -> Result<(), WorkspaceMutationCommitError> {
            let mut state = self.state.lock().expect("fake lock is available");
            let current = expected
                .files()
                .all(|file| state.files.get(file.path.as_str()).cloned() == file.content);
            if !current {
                return Err(WorkspaceMutationCommitError::Conflict);
            }
            if state.fail_commit {
                return Err(WorkspaceMutationCommitError::Filesystem);
            }
            let mut next = state.files.clone();
            for mutation in mutations {
                match mutation {
                    WorkspaceFileMutation::Write { path, content } => {
                        next.insert(String::from(path.as_str()), content.clone());
                    }
                    WorkspaceFileMutation::Delete { path } => {
                        next.remove(path.as_str());
                    }
                }
            }
            state.files = next;
            Ok(())
        }
    }

    fn executor(filesystem: FakeFileSystem) -> WorkspaceMutationExecutor<FakeFileSystem> {
        WorkspaceMutationTools::try_new(filesystem, "/injected")
            .expect("fixture tools construct")
            .into_parts()
            .1
    }

    fn local_executor(
        workspace: &tempfile::TempDir,
    ) -> WorkspaceMutationExecutor<LocalWorkspaceFileSystem> {
        WorkspaceMutationTools::try_new(LocalWorkspaceFileSystem, workspace.path())
            .expect("local fixture tools construct")
            .into_parts()
            .1
    }

    fn immediate_entry_names(workspace: &tempfile::TempDir) -> Vec<String> {
        let mut names = std::fs::read_dir(workspace.path())
            .expect("fixture directory reads")
            .map(|entry| {
                entry
                    .expect("fixture entry reads")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn local_patch_commits_add_update_delete_batch() {
        const KEPT_PATH: &str = "kept.txt";
        const GONE_PATH: &str = "gone.txt";
        const NEW_PATH: &str = "new.txt";
        const UPDATED: &str = "after\n";
        const ADDED: &str = "new\n";

        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        std::fs::write(workspace.path().join(KEPT_PATH), "before\n").expect("kept fixture writes");
        std::fs::write(workspace.path().join(GONE_PATH), "old\n").expect("deleted fixture writes");
        let executor = local_executor(&workspace);
        let patch = parse_patch(
            "*** Begin Patch\n\
             *** Add File: new.txt\n\
             +new\n\
             *** Update File: kept.txt\n\
             @@\n\
             -before\n\
             +after\n\
             *** Delete File: gone.txt\n\
             *** End Patch",
        )
        .expect("structured patch parses");

        let result = executor.apply_patch(&patch).expect("local patch commits");

        assert_eq!(result.operations_applied, 3);
        assert_eq!(
            std::fs::read_to_string(workspace.path().join(KEPT_PATH)).expect("updated file reads"),
            UPDATED
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join(NEW_PATH)).expect("added file reads"),
            ADDED
        );
        assert!(!workspace.path().join(GONE_PATH).exists());
    }

    #[test]
    fn an_initially_missing_parent_is_a_filesystem_failure() {
        let workspace = tempfile::tempdir().expect("workspace constructs");
        let filesystem = LocalWorkspaceFileSystem;
        let root = WorkspaceMutationFileSystem::open_root(&filesystem, workspace.path())
            .expect("root opens");
        let path = WorkspaceMutationPath::try_new("missing/file.txt").expect("path validates");
        let expected = filesystem
            .snapshot(
                &root,
                std::slice::from_ref(&path),
                MAX_WORKSPACE_MUTATION_FILE_BYTES,
            )
            .expect("absent target snapshots");

        let result = filesystem.commit_atomically(
            &root,
            &expected,
            &[WorkspaceFileMutation::Write {
                path,
                content: String::from("tool content"),
            }],
        );

        assert_eq!(result, Err(WorkspaceMutationCommitError::Filesystem));
        assert!(immediate_entry_names(&workspace).is_empty());
    }

    #[test]
    fn a_present_targets_parent_removed_before_staging_is_a_conflict() {
        let workspace = tempfile::tempdir().expect("workspace constructs");
        let parent = workspace.path().join("parent");
        std::fs::create_dir(&parent).expect("parent creates");
        std::fs::write(parent.join("file.txt"), "original").expect("target writes");
        let filesystem = LocalWorkspaceFileSystem;
        let root = WorkspaceMutationFileSystem::open_root(&filesystem, workspace.path())
            .expect("root opens");
        let path = WorkspaceMutationPath::try_new("parent/file.txt").expect("path validates");
        let expected = filesystem
            .snapshot(
                &root,
                std::slice::from_ref(&path),
                MAX_WORKSPACE_MUTATION_FILE_BYTES,
            )
            .expect("present target snapshots");
        std::fs::remove_dir_all(parent).expect("concurrent writer removes parent");

        let error = stage_mutation(
            &root,
            &expected,
            &WorkspaceFileMutation::Write {
                path,
                content: String::from("tool content"),
            },
        )
        .err()
        .expect("missing parent rejects staging");

        assert_eq!(error, WorkspaceMutationCommitError::Conflict);
        assert!(immediate_entry_names(&workspace).is_empty());
    }

    #[test]
    fn local_staging_failure_leaves_every_target_unchanged() {
        const KEPT_PATH: &str = "kept.txt";
        const KEPT_CONTENT: &str = "kept\n";
        const NEW_PATH: &str = "new.txt";
        const EXPECTED_ENTRIES: [&str; 1] = [KEPT_PATH];

        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        std::fs::write(workspace.path().join(KEPT_PATH), KEPT_CONTENT)
            .expect("kept fixture writes");
        let executor = local_executor(&workspace);
        let patch = parse_patch(
            "*** Begin Patch\n\
             *** Add File: new.txt\n\
             +new\n\
             *** Add File: missing/child.txt\n\
             +child\n\
             *** End Patch",
        )
        .expect("structured patch parses");

        let result = executor.apply_patch(&patch);

        assert_eq!(result, Err(MutationFailure::Commit));
        assert_eq!(
            std::fs::read_to_string(workspace.path().join(KEPT_PATH)).expect("kept file reads"),
            KEPT_CONTENT
        );
        assert!(!workspace.path().join(NEW_PATH).exists());
        assert_eq!(immediate_entry_names(&workspace), EXPECTED_ENTRIES);
    }

    #[cfg(unix)]
    #[test]
    fn local_edit_preserves_executable_mode() {
        use std::os::unix::fs::PermissionsExt;

        const PATH: &str = "script.sh";
        const ORIGINAL_MODE: u32 = 0o674;

        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        std::fs::write(workspace.path().join(PATH), "old\n").expect("script fixture writes");
        std::fs::set_permissions(
            workspace.path().join(PATH),
            std::fs::Permissions::from_mode(ORIGINAL_MODE),
        )
        .expect("script fixture mode sets");
        let executor = local_executor(&workspace);

        executor
            .edit_file(
                WorkspaceMutationPath::try_new(PATH).expect("fixture path is valid"),
                "old",
                "new",
                false,
            )
            .expect("local edit succeeds");
        let mode = std::fs::metadata(workspace.path().join(PATH))
            .expect("edited metadata reads")
            .permissions()
            .mode()
            & 0o7777;

        assert_eq!(mode, ORIGINAL_MODE);
    }

    #[cfg(unix)]
    #[test]
    fn local_edit_drops_setuid_and_setgid_mode_bits() {
        use std::os::unix::fs::PermissionsExt;

        const PATH: &str = "privileged.sh";
        const ORIGINAL_MODE: u32 = 0o6674;
        const EXPECTED_MODE: u32 = 0o674;

        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        std::fs::write(workspace.path().join(PATH), "old\n").expect("script fixture writes");
        std::fs::set_permissions(
            workspace.path().join(PATH),
            std::fs::Permissions::from_mode(ORIGINAL_MODE),
        )
        .expect("script fixture mode sets");
        let executor = local_executor(&workspace);
        let fixture_mode = std::fs::metadata(workspace.path().join(PATH))
            .expect("fixture metadata reads")
            .permissions()
            .mode()
            & 0o7777;

        assert_eq!(fixture_mode, ORIGINAL_MODE);

        executor
            .edit_file(
                WorkspaceMutationPath::try_new(PATH).expect("fixture path is valid"),
                "old",
                "new",
                false,
            )
            .expect("local edit succeeds");
        let mode = std::fs::metadata(workspace.path().join(PATH))
            .expect("edited metadata reads")
            .permissions()
            .mode()
            & 0o7777;

        assert_eq!(mode, EXPECTED_MODE);
    }

    #[test]
    fn a_moved_parent_conflicts_at_verification_and_installation() {
        for verify_before_move in [false, true] {
            for replace_parent in [false, true] {
                let workspace = tempfile::tempdir().expect("workspace constructs");
                let outside = tempfile::tempdir().expect("outside directory constructs");
                let parent = workspace.path().join("dir");
                std::fs::create_dir(&parent).expect("parent creates");
                std::fs::write(parent.join("file.txt"), "original").expect("original writes");
                let filesystem = LocalWorkspaceFileSystem;
                let root = WorkspaceMutationFileSystem::open_root(&filesystem, workspace.path())
                    .expect("root opens");
                let path = WorkspaceMutationPath::try_new("dir/file.txt").expect("path validates");
                let expected = filesystem
                    .snapshot(
                        &root,
                        std::slice::from_ref(&path),
                        MAX_WORKSPACE_MUTATION_FILE_BYTES,
                    )
                    .expect("snapshot reads");
                let mut staged = stage_mutation(
                    &root,
                    &expected,
                    &WorkspaceFileMutation::Write {
                        path,
                        content: String::from("tool replacement"),
                    },
                )
                .expect("mutation stages");
                let verified = if verify_before_move {
                    verify_precondition(&root, &expected, &staged).expect("original verifies")
                } else {
                    None
                };
                let moved = outside.path().join("moved");
                std::fs::rename(&parent, &moved).expect("parent moves outside root");
                if replace_parent {
                    std::fs::create_dir(&parent).expect("replacement parent creates");
                    std::fs::write(parent.join("file.txt"), "concurrent writer")
                        .expect("replacement target writes");
                }
                let error = if verify_before_move {
                    install_staged(&root, &mut staged, &expected, verified.as_ref())
                        .expect_err("moved parent conflicts during install")
                } else {
                    verify_precondition(&root, &expected, &staged)
                        .expect_err("moved parent conflicts during verification")
                };
                assert_eq!(
                    rollback_result(std::slice::from_mut(&mut staged), error),
                    WorkspaceMutationCommitError::Conflict
                );
                assert_eq!(
                    std::fs::read_to_string(moved.join("file.txt")).expect("original retained"),
                    "original"
                );
                if replace_parent {
                    assert_eq!(
                        std::fs::read_to_string(parent.join("file.txt"))
                            .expect("replacement retained"),
                        "concurrent writer"
                    );
                } else {
                    assert!(!parent.exists());
                }
            }
        }
    }

    #[test]
    fn backup_replacement_after_the_rollback_precheck_is_preserved() {
        let workspace = tempfile::tempdir().expect("workspace constructs");
        let target = workspace.path().join("file.txt");
        std::fs::write(&target, "original").expect("original writes");
        let filesystem = LocalWorkspaceFileSystem;
        let root = WorkspaceMutationFileSystem::open_root(&filesystem, workspace.path())
            .expect("root opens");
        let path = WorkspaceMutationPath::try_new("file.txt").expect("path validates");
        let expected = filesystem
            .snapshot(
                &root,
                std::slice::from_ref(&path),
                MAX_WORKSPACE_MUTATION_FILE_BYTES,
            )
            .expect("snapshot reads");
        let mut staged = stage_mutation(&root, &expected, &WorkspaceFileMutation::Delete { path })
            .expect("deletion stages");
        let verified = verify_precondition(&root, &expected, &staged).expect("original verifies");
        install_staged(&root, &mut staged, &expected, verified.as_ref())
            .expect("deletion installs");
        assert!(backup_is_owned(&staged));
        let backup = workspace
            .path()
            .join(staged.backup.as_ref().expect("backup retained"));
        let concurrent = workspace.path().join("concurrent.txt");
        std::fs::write(&concurrent, "replacement backup").expect("concurrent file writes");
        std::fs::rename(concurrent, &backup).expect("backup replaced after precheck");

        assert_eq!(restore_backup(&staged), Err(()));
        assert!(!target.exists());
        assert_eq!(
            std::fs::read_to_string(backup).expect("unowned backup retained"),
            "replacement backup"
        );
    }

    #[test]
    fn rollback_preserves_an_unowned_backup_entry_and_the_installed_target() {
        let workspace = tempfile::tempdir().expect("workspace constructs");
        let target = workspace.path().join("file.txt");
        std::fs::write(&target, "original").expect("original writes");
        let filesystem = LocalWorkspaceFileSystem;
        let root = WorkspaceMutationFileSystem::open_root(&filesystem, workspace.path())
            .expect("root opens");
        let path = WorkspaceMutationPath::try_new("file.txt").expect("path validates");
        let expected = filesystem
            .snapshot(
                &root,
                std::slice::from_ref(&path),
                MAX_WORKSPACE_MUTATION_FILE_BYTES,
            )
            .expect("snapshot reads");
        let mut staged = stage_mutation(
            &root,
            &expected,
            &WorkspaceFileMutation::Write {
                path,
                content: String::from("tool replacement"),
            },
        )
        .expect("mutation stages");
        let verified = verify_precondition(&root, &expected, &staged).expect("original verifies");
        install_staged(&root, &mut staged, &expected, verified.as_ref()).expect("tool installs");
        let backup = workspace
            .path()
            .join(staged.backup.as_ref().expect("backup retained"));
        let concurrent = workspace.path().join("concurrent.txt");
        std::fs::write(&concurrent, "replacement backup").expect("concurrent file writes");
        std::fs::rename(concurrent, &backup).expect("concurrent writer replaces backup entry");

        assert_eq!(
            rollback_result(
                std::slice::from_mut(&mut staged),
                WorkspaceMutationCommitError::Conflict
            ),
            WorkspaceMutationCommitError::Ambiguous
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("installed target retained"),
            "tool replacement"
        );
        assert_eq!(
            std::fs::read_to_string(&backup).expect("unowned backup retained"),
            "replacement backup"
        );
    }

    #[test]
    fn a_target_deleted_before_precondition_verification_is_a_conflict() {
        let workspace = tempfile::tempdir().expect("workspace constructs");
        let target = workspace.path().join("file.txt");
        std::fs::write(&target, "original").expect("original writes");
        let filesystem = LocalWorkspaceFileSystem;
        let root = WorkspaceMutationFileSystem::open_root(&filesystem, workspace.path())
            .expect("root opens");
        let path = WorkspaceMutationPath::try_new("file.txt").expect("path validates");
        let expected = filesystem
            .snapshot(
                &root,
                std::slice::from_ref(&path),
                MAX_WORKSPACE_MUTATION_FILE_BYTES,
            )
            .expect("snapshot reads");
        let staged = stage_mutation(&root, &expected, &WorkspaceFileMutation::Delete { path })
            .expect("deletion stages");
        std::fs::remove_file(&target).expect("concurrent writer deletes target");

        assert_eq!(
            verify_precondition(&root, &expected, &staged).expect_err("deleted target conflicts"),
            WorkspaceMutationCommitError::Conflict
        );
    }

    #[test]
    fn rollback_preserves_a_concurrent_replacement_of_an_installed_target() {
        let workspace = tempfile::tempdir().expect("workspace constructs");
        let target = workspace.path().join("file.txt");
        std::fs::write(&target, "original").expect("original writes");
        let filesystem = LocalWorkspaceFileSystem;
        let root = WorkspaceMutationFileSystem::open_root(&filesystem, workspace.path())
            .expect("root opens");
        let path = WorkspaceMutationPath::try_new("file.txt").expect("path validates");
        let expected = filesystem
            .snapshot(
                &root,
                std::slice::from_ref(&path),
                MAX_WORKSPACE_MUTATION_FILE_BYTES,
            )
            .expect("snapshot reads");
        let mut staged = stage_mutation(
            &root,
            &expected,
            &WorkspaceFileMutation::Write {
                path,
                content: String::from("tool replacement"),
            },
        )
        .expect("mutation stages");
        let verified = verify_precondition(&root, &expected, &staged).expect("original verifies");
        install_staged(&root, &mut staged, &expected, verified.as_ref()).expect("tool installs");
        let backup = workspace
            .path()
            .join(staged.backup.as_ref().expect("backup retained"));
        let concurrent = workspace.path().join("concurrent.txt");
        std::fs::write(&concurrent, "concurrent writer").expect("concurrent file writes");
        std::fs::rename(concurrent, &target).expect("concurrent writer replaces installed target");

        assert_eq!(
            rollback_result(
                std::slice::from_mut(&mut staged),
                WorkspaceMutationCommitError::Conflict
            ),
            WorkspaceMutationCommitError::Ambiguous
        );
        assert_eq!(
            std::fs::read_to_string(target).expect("target reads"),
            "concurrent writer"
        );
        assert_eq!(
            std::fs::read_to_string(backup).expect("backup retained"),
            "original"
        );
    }

    #[test]
    fn rollback_preserves_an_in_place_rewrite_with_unchanged_size_and_mtime() {
        let workspace = tempfile::tempdir().expect("workspace constructs");
        let target = workspace.path().join("file.txt");
        std::fs::write(&target, "original").expect("original writes");
        let filesystem = LocalWorkspaceFileSystem;
        let root = WorkspaceMutationFileSystem::open_root(&filesystem, workspace.path())
            .expect("root opens");
        let path = WorkspaceMutationPath::try_new("file.txt").expect("path validates");
        let expected = filesystem
            .snapshot(
                &root,
                std::slice::from_ref(&path),
                MAX_WORKSPACE_MUTATION_FILE_BYTES,
            )
            .expect("snapshot reads");
        let mut staged = stage_mutation(
            &root,
            &expected,
            &WorkspaceFileMutation::Write {
                path,
                content: String::from("tool replacement"),
            },
        )
        .expect("mutation stages");
        let verified = verify_precondition(&root, &expected, &staged).expect("original verifies");
        install_staged(&root, &mut staged, &expected, verified.as_ref()).expect("tool installs");
        let backup = workspace
            .path()
            .join(staged.backup.as_ref().expect("backup retained"));
        let installed_mtime = std::fs::metadata(&target)
            .expect("metadata reads")
            .modified()
            .expect("modification time reads");
        // The competing bytes keep the inode and size, and the writer restores the installed mtime.
        std::fs::write(&target, "concurrent write").expect("concurrent writer rewrites in place");
        File::options()
            .write(true)
            .open(&target)
            .expect("target opens")
            .set_times(std::fs::FileTimes::new().set_modified(installed_mtime))
            .expect("installed modification time is restored");

        assert_eq!(
            rollback_result(
                std::slice::from_mut(&mut staged),
                WorkspaceMutationCommitError::Conflict
            ),
            WorkspaceMutationCommitError::Ambiguous
        );
        assert_eq!(
            std::fs::read_to_string(target).expect("target reads"),
            "concurrent write"
        );
        assert_eq!(
            std::fs::read_to_string(backup).expect("backup retained"),
            "original"
        );
    }

    #[test]
    fn concurrent_creation_of_an_absent_target_is_a_conflict() {
        for create_before_verification in [false, true] {
            let workspace = tempfile::tempdir().expect("workspace constructs");
            let target = workspace.path().join("file.txt");
            let filesystem = LocalWorkspaceFileSystem;
            let root = WorkspaceMutationFileSystem::open_root(&filesystem, workspace.path())
                .expect("root opens");
            let path = WorkspaceMutationPath::try_new("file.txt").expect("path validates");
            let expected = filesystem
                .snapshot(
                    &root,
                    std::slice::from_ref(&path),
                    MAX_WORKSPACE_MUTATION_FILE_BYTES,
                )
                .expect("absent target snapshots");
            let mut staged = stage_mutation(
                &root,
                &expected,
                &WorkspaceFileMutation::Write {
                    path,
                    content: String::from("tool content"),
                },
            )
            .expect("creation stages");
            if create_before_verification {
                std::fs::write(&target, "concurrent writer").expect("concurrent target creates");
            }
            let verified = verify_precondition(&root, &expected, &staged)
                .expect("creation defers absence check to installation");
            if !create_before_verification {
                std::fs::write(&target, "concurrent writer").expect("concurrent target creates");
            }

            let error = install_staged(&root, &mut staged, &expected, verified.as_ref())
                .expect_err("concurrent creation conflicts");
            assert_eq!(
                rollback_result(std::slice::from_mut(&mut staged), error),
                WorkspaceMutationCommitError::Conflict
            );
            assert_eq!(
                std::fs::read_to_string(&target).expect("concurrent target retained"),
                "concurrent writer"
            );
            assert_eq!(
                std::fs::read_dir(workspace.path())
                    .expect("workspace reads")
                    .count(),
                1
            );
        }
    }

    #[test]
    fn rollback_preserves_a_concurrent_replacement_of_a_created_target() {
        let workspace = tempfile::tempdir().expect("workspace constructs");
        let target = workspace.path().join("file.txt");
        let filesystem = LocalWorkspaceFileSystem;
        let root = WorkspaceMutationFileSystem::open_root(&filesystem, workspace.path())
            .expect("root opens");
        let path = WorkspaceMutationPath::try_new("file.txt").expect("path validates");
        let expected = filesystem
            .snapshot(
                &root,
                std::slice::from_ref(&path),
                MAX_WORKSPACE_MUTATION_FILE_BYTES,
            )
            .expect("snapshot reads");
        let mut staged = stage_mutation(
            &root,
            &expected,
            &WorkspaceFileMutation::Write {
                path,
                content: String::from("tool creation"),
            },
        )
        .expect("mutation stages");
        install_staged(&root, &mut staged, &expected, None).expect("tool creates target");
        let concurrent = workspace.path().join("concurrent.txt");
        std::fs::write(&concurrent, "concurrent writer").expect("concurrent file writes");
        std::fs::rename(concurrent, &target).expect("concurrent writer replaces created target");

        assert_eq!(
            rollback_result(
                std::slice::from_mut(&mut staged),
                WorkspaceMutationCommitError::Conflict
            ),
            WorkspaceMutationCommitError::Ambiguous
        );
        assert_eq!(
            std::fs::read_to_string(target).expect("target reads"),
            "concurrent writer"
        );
    }

    #[test]
    fn replacement_between_verification_and_install_is_preserved() {
        for concurrent_content in ["original", "concurrent writer"] {
            let workspace = tempfile::tempdir().expect("workspace constructs");
            let target = workspace.path().join("file.txt");
            std::fs::write(&target, "original").expect("original writes");
            let filesystem = LocalWorkspaceFileSystem;
            let root = WorkspaceMutationFileSystem::open_root(&filesystem, workspace.path())
                .expect("root opens");
            let path = WorkspaceMutationPath::try_new("file.txt").expect("path validates");
            let expected = filesystem
                .snapshot(
                    &root,
                    std::slice::from_ref(&path),
                    MAX_WORKSPACE_MUTATION_FILE_BYTES,
                )
                .expect("snapshot reads");
            let mut staged = stage_mutation(
                &root,
                &expected,
                &WorkspaceFileMutation::Write {
                    path,
                    content: String::from("tool replacement"),
                },
            )
            .expect("mutation stages");
            let verified =
                verify_precondition(&root, &expected, &staged).expect("original verifies");
            let concurrent = workspace.path().join("concurrent.txt");
            std::fs::write(&concurrent, concurrent_content).expect("concurrent file writes");
            std::fs::rename(concurrent, &target).expect("concurrent writer replaces target");

            let error = install_staged(&root, &mut staged, &expected, verified.as_ref())
                .expect_err("replacement conflicts");
            assert_eq!(
                rollback_result(std::slice::from_mut(&mut staged), error),
                WorkspaceMutationCommitError::Conflict
            );
            assert_eq!(
                std::fs::read_to_string(target).expect("target reads"),
                concurrent_content
            );
        }
    }

    #[test]
    fn in_place_write_between_verification_and_install_is_preserved() {
        let workspace = tempfile::tempdir().expect("workspace constructs");
        let target = workspace.path().join("file.txt");
        std::fs::write(&target, "original").expect("original writes");
        let filesystem = LocalWorkspaceFileSystem;
        let root = WorkspaceMutationFileSystem::open_root(&filesystem, workspace.path())
            .expect("root opens");
        let path = WorkspaceMutationPath::try_new("file.txt").expect("path validates");
        let expected = filesystem
            .snapshot(
                &root,
                std::slice::from_ref(&path),
                MAX_WORKSPACE_MUTATION_FILE_BYTES,
            )
            .expect("snapshot reads");
        let mut staged = stage_mutation(&root, &expected, &WorkspaceFileMutation::Delete { path })
            .expect("deletion stages");
        let verified = verify_precondition(&root, &expected, &staged).expect("original verifies");
        std::fs::write(&target, "concurrent writer")
            .expect("concurrent writer changes inode contents");

        let error = install_staged(&root, &mut staged, &expected, verified.as_ref())
            .expect_err("changed contents conflict");
        assert_eq!(
            rollback_result(std::slice::from_mut(&mut staged), error),
            WorkspaceMutationCommitError::Conflict
        );
        assert_eq!(
            std::fs::read_to_string(target).expect("target reads"),
            "concurrent writer"
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_snapshot_rejects_intermediate_escaping_symlink() {
        use std::os::unix::fs::symlink;

        const ESCAPE_PATH: &str = "escape/new.txt";

        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        let outside = tempfile::tempdir().expect("outside fixture constructs");
        symlink(outside.path(), workspace.path().join("escape"))
            .expect("escaping directory symlink fixture constructs");
        let executor = local_executor(&workspace);
        let path = WorkspaceMutationPath::try_new(ESCAPE_PATH).expect("fixture path is valid");

        let error = executor
            .filesystem
            .snapshot(
                &executor.root,
                std::slice::from_ref(&path),
                MAX_WORKSPACE_MUTATION_FILE_BYTES,
            )
            .expect_err("intermediate escaping symlink rejects");

        assert_eq!(error.path, Some(path));
        assert_eq!(
            error.kind,
            WorkspaceMutationSnapshotErrorKind::PathRejected(WorkspacePathRejection::Symlink)
        );
        assert!(!outside.path().join("new.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn local_commit_rejects_target_swapped_to_escaping_symlink() {
        use std::os::unix::fs::symlink;

        const PATH: &str = "file.txt";
        const ORIGINAL: &str = "original\n";
        const OUTSIDE: &str = "outside\n";

        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        let outside = tempfile::tempdir().expect("outside fixture constructs");
        let workspace_path = workspace.path().join(PATH);
        let outside_path = outside.path().join(PATH);
        std::fs::write(&workspace_path, ORIGINAL).expect("workspace fixture writes");
        std::fs::write(&outside_path, OUTSIDE).expect("outside fixture writes");
        let filesystem = LocalWorkspaceFileSystem;
        let root = WorkspaceMutationFileSystem::open_root(&filesystem, workspace.path())
            .expect("fixture root opens");
        let path = WorkspaceMutationPath::try_new(PATH).expect("fixture path is valid");
        let expected = filesystem
            .snapshot(
                &root,
                std::slice::from_ref(&path),
                MAX_WORKSPACE_MUTATION_FILE_BYTES,
            )
            .expect("initial snapshot succeeds");
        std::fs::remove_file(&workspace_path).expect("workspace target removes");
        symlink(&outside_path, &workspace_path).expect("replacement symlink constructs");

        let error = filesystem
            .commit_atomically(
                &root,
                &expected,
                &[WorkspaceFileMutation::Write {
                    path: path.clone(),
                    content: String::from("replacement\n"),
                }],
            )
            .expect_err("swapped target rejects");

        assert_eq!(
            error,
            WorkspaceMutationCommitError::PathRejected {
                path,
                reason: WorkspacePathRejection::Symlink,
            }
        );
        assert_eq!(
            std::fs::read_to_string(outside_path).expect("outside target reads"),
            OUTSIDE
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_snapshot_rejects_escaping_symlink() {
        use std::os::unix::fs::symlink;

        const ESCAPE_PATH: &str = "escape.txt";

        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        let outside = tempfile::tempdir().expect("outside fixture constructs");
        let outside_path = outside.path().join("secret.txt");
        std::fs::write(&outside_path, "secret").expect("outside fixture writes");
        symlink(&outside_path, workspace.path().join(ESCAPE_PATH))
            .expect("escaping symlink fixture constructs");
        let executor = local_executor(&workspace);
        let path = WorkspaceMutationPath::try_new(ESCAPE_PATH).expect("fixture path is valid");

        let error = executor
            .filesystem
            .snapshot(
                &executor.root,
                std::slice::from_ref(&path),
                MAX_WORKSPACE_MUTATION_FILE_BYTES,
            )
            .expect_err("escaping symlink rejects");

        assert_eq!(error.path, Some(path));
        assert_eq!(
            error.kind,
            WorkspaceMutationSnapshotErrorKind::PathRejected(WorkspacePathRejection::Symlink)
        );
    }

    #[test]
    fn write_file_creates_complete_content() {
        const PATH: &str = "new.txt";
        const CONTENT: &str = "new\n";

        let filesystem = FakeFileSystem::default();
        let executor = executor(filesystem.clone());
        let result = executor
            .write_file(
                WorkspaceMutationPath::try_new(PATH).expect("fixture path is valid"),
                String::from(CONTENT),
            )
            .expect("write succeeds");

        assert_eq!(
            filesystem.files().get(PATH).map(String::as_str),
            Some(CONTENT)
        );
        assert!(result.created);
        assert_eq!(result.bytes_written, CONTENT.len());
    }

    #[test]
    fn write_file_overwrites_complete_content() {
        const PATH: &str = "existing.txt";
        const ORIGINAL: &str = "old\n";
        const REPLACEMENT: &str = "replacement\n";

        let filesystem = FakeFileSystem::with_files([(PATH, ORIGINAL)]);
        let executor = executor(filesystem.clone());
        let result = executor
            .write_file(
                WorkspaceMutationPath::try_new(PATH).expect("fixture path is valid"),
                String::from(REPLACEMENT),
            )
            .expect("overwrite succeeds");

        assert_eq!(
            filesystem.files().get(PATH).map(String::as_str),
            Some(REPLACEMENT)
        );
        assert!(!result.created);
        assert_eq!(result.bytes_written, REPLACEMENT.len());
    }

    #[test]
    fn edit_file_requires_unique_match_by_default() {
        const PATH: &str = "file.txt";
        const CONTENT: &str = "same same";

        let filesystem = FakeFileSystem::with_files([(PATH, CONTENT)]);
        let executor = executor(filesystem.clone());
        let original = filesystem.files();
        let result = executor.edit_file(
            WorkspaceMutationPath::try_new(PATH).expect("fixture path is valid"),
            "same",
            "new",
            false,
        );

        assert_eq!(result, Err(MutationFailure::EditMatch));
        assert_eq!(filesystem.files(), original);
    }

    #[test]
    fn edit_file_rejects_missing_match_by_default() {
        const PATH: &str = "file.txt";
        const CONTENT: &str = "present";

        let filesystem = FakeFileSystem::with_files([(PATH, CONTENT)]);
        let executor = executor(filesystem.clone());
        let original = filesystem.files();
        let result = executor.edit_file(
            WorkspaceMutationPath::try_new(PATH).expect("fixture path is valid"),
            "missing",
            "new",
            false,
        );

        assert_eq!(result, Err(MutationFailure::EditMatch));
        assert_eq!(filesystem.files(), original);
    }

    #[test]
    fn edit_file_replaces_one_unique_match_by_default() {
        const PATH: &str = "file.txt";
        const EXPECTED: &str = "before new after";

        let filesystem = FakeFileSystem::with_files([(PATH, "before old after")]);
        let executor = executor(filesystem.clone());
        let result = executor
            .edit_file(
                WorkspaceMutationPath::try_new(PATH).expect("fixture path is valid"),
                "old",
                "new",
                false,
            )
            .expect("unique replacement succeeds");

        assert_eq!(result.replacements, 1);
        assert_eq!(
            filesystem.files().get(PATH).map(String::as_str),
            Some(EXPECTED)
        );
    }

    #[test]
    fn edit_file_rejects_overlapping_matches_by_default() {
        const PATH: &str = "file.txt";
        const CONTENT: &str = "aaa";

        let filesystem = FakeFileSystem::with_files([(PATH, CONTENT)]);
        let executor = executor(filesystem.clone());
        let original = filesystem.files();
        let result = executor.edit_file(
            WorkspaceMutationPath::try_new(PATH).expect("fixture path is valid"),
            "aa",
            "new",
            false,
        );

        assert_eq!(result, Err(MutationFailure::EditMatch));
        assert_eq!(filesystem.files(), original);
    }

    #[test]
    fn edit_file_rejects_output_over_byte_cap() {
        const PATH: &str = "file.txt";

        let content = "a".repeat(MAX_WORKSPACE_MUTATION_FILE_BYTES);
        let filesystem = FakeFileSystem::with_files([(PATH, "placeholder")]);
        filesystem
            .state
            .lock()
            .expect("fake lock is available")
            .files
            .insert(String::from(PATH), content);
        let executor = executor(filesystem.clone());
        let original = filesystem.files();
        let result = executor.edit_file(
            WorkspaceMutationPath::try_new(PATH).expect("fixture path is valid"),
            "a",
            "aa",
            true,
        );

        assert_eq!(result, Err(MutationFailure::EditMatch));
        assert_eq!(filesystem.files(), original);
    }

    #[test]
    fn edit_file_replace_all_rejects_huge_expansion_before_allocation() {
        const PATH: &str = "file.txt";
        const MATCH: &str = "a";
        const REPLACEMENT_UNIT: &str = "b";

        let source = MATCH.repeat(MAX_WORKSPACE_MUTATION_FILE_BYTES);
        let replacement = REPLACEMENT_UNIT.repeat(MAX_WORKSPACE_MUTATION_FILE_BYTES);
        let filesystem = FakeFileSystem::default();
        filesystem
            .state
            .lock()
            .expect("fake lock is available")
            .files
            .insert(String::from(PATH), source);
        let executor = executor(filesystem.clone());
        let original = filesystem.files();
        let result = executor.edit_file(
            WorkspaceMutationPath::try_new(PATH).expect("fixture path is valid"),
            MATCH,
            &replacement,
            true,
        );

        assert_eq!(result, Err(MutationFailure::EditMatch));
        assert_eq!(filesystem.files(), original);
    }

    #[test]
    fn replacement_output_length_rejects_arithmetic_overflow() {
        assert_eq!(replacement_output_bytes(usize::MAX, 1, usize::MAX, 2), None);
    }

    #[test]
    fn edit_file_replace_all_is_explicit() {
        const PATH: &str = "file.txt";
        const EXPECTED: &str = "new new";

        let filesystem = FakeFileSystem::with_files([(PATH, "same same")]);
        let executor = executor(filesystem.clone());
        let result = executor
            .edit_file(
                WorkspaceMutationPath::try_new(PATH).expect("fixture path is valid"),
                "same",
                "new",
                true,
            )
            .expect("replace-all succeeds");

        assert_eq!(result.replacements, 2);
        assert_eq!(
            filesystem.files().get(PATH).map(String::as_str),
            Some(EXPECTED)
        );
    }

    #[test]
    fn patch_context_failure_leaves_every_file_unchanged() {
        const FIRST_PATH: &str = "first.txt";
        const SECOND_PATH: &str = "second.txt";

        let filesystem =
            FakeFileSystem::with_files([(FIRST_PATH, "old\n"), (SECOND_PATH, "present\n")]);
        let executor = executor(filesystem.clone());
        let original = filesystem.files();
        let patch = parse_patch(
            "*** Begin Patch\n\
             *** Update File: first.txt\n\
             @@\n\
             -old\n\
             +new\n\
             *** Update File: second.txt\n\
             @@\n\
             -missing\n\
             +replacement\n\
             *** End Patch",
        )
        .expect("structured patch parses");

        let result = executor.apply_patch(&patch);

        let Err(MutationFailure::Patch(detail)) = result else {
            panic!("context failure returns patch detail")
        };
        assert!(detail.contains("hunk 1"));
        assert!(detail.contains(SECOND_PATH));
        assert_eq!(filesystem.files(), original);
    }
}
