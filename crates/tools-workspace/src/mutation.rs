use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolArgumentValidator, ToolExecutionInvocation, ToolExecutor,
    ToolExecutorEvidence,
};
use signalbox_domain::{
    NormalizedToolArguments, ToolEffectClass, ToolExecutionErrorDetail, ToolPermissionDefault,
};
use signalbox_tool_contract::{
    ToolContract, ToolContractCompileError, compile_contract_definition,
};

use crate::{
    PlannedPatchOperation, WorkspacePatch, WorkspacePathRejection, parse_patch, plan_patch,
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
const COMMIT_FAILED_DETAIL: &str = "atomic workspace mutation commit failed";

/// Typed `write_file` arguments.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteFileArguments {
    /// Relative destination path inside the injected root.
    #[schemars(length(min = 1, max = crate::path::MAX_WORKSPACE_PATH_BYTES))]
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
    #[schemars(length(min = 1, max = crate::path::MAX_WORKSPACE_PATH_BYTES))]
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
    /// Structured add/update/delete patch bounded to one MiB.
    #[schemars(length(min = 1, max = crate::MAX_PATCH_BYTES))]
    pub patch: String,
}

struct ApplyPatchContract;

impl ToolContract for ApplyPatchContract {
    type Arguments = ApplyPatchArguments;
    const NAME: &'static str = APPLY_PATCH_NAME;
    const DESCRIPTION: &'static str =
        "Atomically applies one structured add/update/delete patch inside the workspace root.";
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

/// One file value captured before an atomic mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceFileSnapshot {
    /// Relative workspace path.
    pub path: WorkspaceMutationPath,
    /// Complete UTF-8 content, or `None` when the path did not exist.
    pub content: Option<String>,
}

/// A complete precondition snapshot for all paths touched by one operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceMutationSnapshot {
    files: BTreeMap<WorkspaceMutationPath, Option<String>>,
}

impl WorkspaceMutationSnapshot {
    /// Builds a snapshot while rejecting duplicate path evidence.
    pub fn try_new(
        files: impl IntoIterator<Item = WorkspaceFileSnapshot>,
    ) -> Result<Self, WorkspaceMutationSnapshotError> {
        let mut collected = BTreeMap::new();
        for file in files {
            match collected.entry(file.path) {
                std::collections::btree_map::Entry::Vacant(entry) => {
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
        Ok(Self { files: collected })
    }

    /// Returns captured content for one requested path.
    pub fn content(&self, path: &WorkspaceMutationPath) -> Option<&Option<String>> {
        self.files.get(path)
    }

    /// Iterates captured files in lexical path order.
    pub fn files(&self) -> impl Iterator<Item = WorkspaceFileSnapshot> + '_ {
        self.files
            .iter()
            .map(|(path, content)| WorkspaceFileSnapshot {
                path: path.clone(),
                content: content.clone(),
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

/// Why an adapter could not commit a prevalidated mutation batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceMutationCommitError {
    /// A captured precondition changed before commit.
    Conflict,
    /// A path escaped or violated the injected-root boundary.
    PathRejected {
        /// Affected relative path.
        path: WorkspaceMutationPath,
        /// Typed rejection evidence.
        reason: WorkspacePathRejection,
    },
    /// The filesystem could not apply the whole batch atomically.
    Filesystem,
}

impl fmt::Display for WorkspaceMutationCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict => formatter.write_str("workspace changed before atomic commit"),
            Self::PathRejected { path, reason } => {
                write!(
                    formatter,
                    "workspace path {:?} rejected: {reason}",
                    path.as_str()
                )
            }
            Self::Filesystem => formatter.write_str("atomic workspace commit failed"),
        }
    }
}

impl Error for WorkspaceMutationCommitError {}

/// Injected atomic filesystem authority for workspace mutation tools.
///
/// Implementations must resolve every `path` relative to `root` without
/// reopening an ambient absolute pathname. `snapshot` must reject symlinks and
/// non-regular files. `commit_atomically` must either apply every mutation or
/// leave every captured path unchanged, and must reject changed preconditions.
pub trait WorkspaceMutationFileSystem: Clone + Send + Sync + 'static {
    /// Captures complete bounded values for every requested path.
    fn snapshot(
        &self,
        root: &Path,
        paths: &[WorkspaceMutationPath],
        max_file_bytes: usize,
    ) -> Result<WorkspaceMutationSnapshot, WorkspaceMutationSnapshotError>;

    /// Atomically commits a prevalidated batch against its captured values.
    fn commit_atomically(
        &self,
        root: &Path,
        expected: &WorkspaceMutationSnapshot,
        mutations: &[WorkspaceFileMutation],
    ) -> Result<(), WorkspaceMutationCommitError>;
}

/// A static mutation declaration could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceMutationToolConstructionError {
    /// One static contract name was invalid.
    Name,
    /// One static contract schema was invalid.
    Schema,
    /// One static error detail was invalid.
    ErrorDetail,
    /// The catalog unexpectedly contained a duplicate.
    Duplicate,
}

impl fmt::Display for WorkspaceMutationToolConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Name => "workspace mutation-tool static name is invalid",
            Self::Schema => "workspace mutation-tool static schema is invalid",
            Self::ErrorDetail => "workspace mutation-tool static detail is invalid",
            Self::Duplicate => "workspace mutation-tool catalog is duplicated",
        })
    }
}

impl Error for WorkspaceMutationToolConstructionError {}

/// Compiled mutation catalog and executor around one injected root authority.
pub struct WorkspaceMutationTools<FileSystem> {
    catalog: CompiledToolCatalog,
    executor: WorkspaceMutationExecutor<FileSystem>,
}

impl<FileSystem: WorkspaceMutationFileSystem> WorkspaceMutationTools<FileSystem> {
    /// Compiles the three mutation tools around an injected filesystem/root.
    pub fn try_new(
        filesystem: FileSystem,
        root: impl AsRef<Path>,
    ) -> Result<Self, WorkspaceMutationToolConstructionError> {
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
                root: root.as_ref().to_owned(),
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
            .map_err(|_| self.detail.clone())
    }
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

fn decode_operation(
    kind: MutationToolKind,
    arguments: &NormalizedToolArguments,
) -> Result<MutationOperation, ()> {
    match kind {
        MutationToolKind::WriteFile => {
            let decoded: WriteFileArguments =
                serde_json::from_str(arguments.as_str()).map_err(drop)?;
            if decoded.content.len() > MAX_WORKSPACE_MUTATION_FILE_BYTES {
                return Err(());
            }
            let path = WorkspaceMutationPath::try_new(decoded.path).map_err(drop)?;
            Ok(MutationOperation::Write {
                path,
                content: decoded.content,
            })
        }
        MutationToolKind::EditFile => {
            let decoded: EditFileArguments =
                serde_json::from_str(arguments.as_str()).map_err(drop)?;
            if decoded.old_string.is_empty()
                || decoded.old_string.len() > MAX_WORKSPACE_MUTATION_FILE_BYTES
                || decoded.new_string.len() > MAX_WORKSPACE_MUTATION_FILE_BYTES
            {
                return Err(());
            }
            let path = WorkspaceMutationPath::try_new(decoded.path).map_err(drop)?;
            Ok(MutationOperation::Edit {
                path,
                old_string: decoded.old_string,
                new_string: decoded.new_string,
                replace_all: decoded.replace_all,
            })
        }
        MutationToolKind::ApplyPatch => {
            let decoded: ApplyPatchArguments =
                serde_json::from_str(arguments.as_str()).map_err(drop)?;
            parse_patch(&decoded.patch)
                .map(MutationOperation::ApplyPatch)
                .map_err(drop)
        }
    }
}

fn drop<T>(_value: T) {}

/// Executor for the three approved workspace mutation tools.
pub struct WorkspaceMutationExecutor<FileSystem> {
    filesystem: FileSystem,
    root: PathBuf,
    snapshot_failed_detail: ToolExecutionErrorDetail,
    edit_match_failed_detail: ToolExecutionErrorDetail,
    patch_failed_detail: ToolExecutionErrorDetail,
    commit_failed_detail: ToolExecutionErrorDetail,
}

/// A checked catalog/executor assumption failed inside the mutation family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceMutationExecutorError {
    /// Executor argument decoding disagreed with catalog validation.
    ArgumentValidationDrift,
    /// Compact result encoding unexpectedly failed.
    ResultEncoding,
}

impl fmt::Display for WorkspaceMutationExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ArgumentValidationDrift => "workspace mutation argument validation drifted",
            Self::ResultEncoding => "workspace mutation result encoding failed",
        })
    }
}

impl Error for WorkspaceMutationExecutorError {}

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
            .map_err(|()| WorkspaceMutationExecutorError::ArgumentValidationDrift)?;
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
            Err(MutationFailure::Patch) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.patch_failed_detail.clone()),
            },
            Err(MutationFailure::Commit) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.commit_failed_detail.clone()),
            },
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MutationFailure {
    Snapshot,
    EditMatch,
    Patch,
    Commit,
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
            .map_err(|_| MutationFailure::Commit)
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
        let matches = source.match_indices(old_string).count();
        if matches == 0 || !replace_all && matches != 1 {
            return Err(MutationFailure::EditMatch);
        }
        let content = if replace_all {
            source.replace(old_string, new_string)
        } else {
            source.replacen(old_string, new_string, 1)
        };
        if content.len() > MAX_WORKSPACE_MUTATION_FILE_BYTES {
            return Err(MutationFailure::EditMatch);
        }
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
            .map_err(|_| MutationFailure::Patch)?;
        let snapshot = self.capture(&paths)?;
        let contents = snapshot
            .files()
            .filter_map(|file| file.content.map(|content| (file.path.0, content)))
            .collect::<BTreeMap<_, _>>();
        let plan = plan_patch(patch, &contents).map_err(|_| MutationFailure::Patch)?;
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
                return Err(MutationFailure::Patch);
            }
            Ok(WorkspaceFileMutation::Write {
                path: WorkspaceMutationPath::try_new(path.clone())
                    .map_err(|_| MutationFailure::Patch)?,
                content: content.clone(),
            })
        }
        PlannedPatchOperation::Delete { path } => Ok(WorkspaceFileMutation::Delete {
            path: WorkspaceMutationPath::try_new(path.clone())
                .map_err(|_| MutationFailure::Patch)?,
        }),
    }
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
    /// Number of file operations committed atomically.
    pub operations_applied: usize,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use signalbox_application::ToolCatalog;

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
        fn snapshot(
            &self,
            _root: &Path,
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
                    Ok(WorkspaceFileSnapshot {
                        path: path.clone(),
                        content,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            WorkspaceMutationSnapshot::try_new(files)
        }

        fn commit_atomically(
            &self,
            _root: &Path,
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

    #[test]
    fn mutation_definitions_require_confirmation_and_external_effects() {
        let tools = WorkspaceMutationTools::try_new(FakeFileSystem::default(), "/injected")
            .expect("fixture tools construct");
        let (catalog, _executor) = tools.into_parts();
        let definitions = catalog.definitions();

        assert_eq!(definitions.len(), WORKSPACE_MUTATION_TOOL_NAMES.len());
        assert!(definitions.iter().all(|definition| {
            definition.permission_default() == ToolPermissionDefault::Confirm
        }));
        assert!(
            definitions
                .iter()
                .all(|definition| { definition.effect_class() == ToolEffectClass::ExternalEffect })
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

        assert_eq!(result, Err(MutationFailure::Patch));
        assert_eq!(filesystem.files(), original);
    }

    #[test]
    fn patch_commits_add_update_delete_as_one_batch() {
        const KEPT_PATH: &str = "kept.txt";
        const NEW_PATH: &str = "new.txt";
        const GONE_PATH: &str = "gone.txt";
        const UPDATED: &str = "after\n";
        const ADDED: &str = "new\n";

        let filesystem =
            FakeFileSystem::with_files([(KEPT_PATH, "before\n"), (GONE_PATH, "old\n")]);
        let executor = executor(filesystem.clone());
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

        let result = executor.apply_patch(&patch).expect("patch commits");
        let files = filesystem.files();

        assert_eq!(result.operations_applied, 3);
        assert_eq!(files.get(KEPT_PATH).map(String::as_str), Some(UPDATED));
        assert_eq!(files.get(NEW_PATH).map(String::as_str), Some(ADDED));
        assert!(!files.contains_key(GONE_PATH));
    }
}
