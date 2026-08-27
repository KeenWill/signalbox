//! `read_file`, `list_directory`, `glob_files`, and `search_files`: the
//! read-only workspace tool catalog (`WorkspaceReadTools`), each bounded by
//! byte, entry, and result-count limits enforced against the injected
//! `WorkspaceFileSystem`.

use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

use glob::Pattern;
use regex::Regex;
use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolArgumentValidator, ToolExecutionInvocation, ToolExecutor,
    ToolExecutorEvidence,
};
use signalbox_domain::{
    NormalizedToolArguments, ToolEffectClass, ToolExecutionErrorDetail, ToolPermissionDefault,
    ToolResultText, ToolResultTextFailure,
};
use signalbox_tool_contract::{
    ToolContract, ToolContractCompileError, compile_contract_definition,
};

use crate::path::{
    WorkspaceEntryKind, WorkspaceFileSystem, WorkspacePathRejection, WorkspaceResolveError,
    WorkspaceRoot, WorkspaceRootError, validate_relative_path,
};

pub const READ_FILE_NAME: &str = "read_file";
pub const LIST_DIRECTORY_NAME: &str = "list_directory";
pub const GLOB_FILES_NAME: &str = "glob_files";
pub const SEARCH_FILES_NAME: &str = "search_files";

/// Stable read-family registry names in declaration order.
pub const WORKSPACE_READ_TOOL_NAMES: [&str; 4] = [
    READ_FILE_NAME,
    LIST_DIRECTORY_NAME,
    GLOB_FILES_NAME,
    SEARCH_FILES_NAME,
];

const DEFAULT_READ_MAX_BYTES: usize = 32 * 1024;
/// Maximum content bytes one `read_file` call retains.
///
/// This leaves model-context room for the surrounding prompt and parallel
/// tool results instead of allowing one read to consume an entire call. It
/// bounds one call rather than the file: `ReadFileArguments::offset` reaches
/// content past this window without raising the per-call ceiling.
pub const MAX_WORKSPACE_READ_BYTES: usize = 32 * 1024;
const DEFAULT_MAX_RESULTS: usize = 100;
const MAX_RESULTS: usize = 256;
const MAX_PATTERN_CHARACTERS: usize = 4096;
const MAX_PATTERN_BYTES: usize = MAX_PATTERN_CHARACTERS * 4;
const MAX_SEARCH_FILE_BYTES: usize = 1024 * 1024;
const MAX_SEARCH_TOTAL_BYTES: usize = 4 * 1024 * 1024;
const MAX_SEARCH_LINE_BYTES: usize = 1024;
const MAX_WALK_ENTRIES: usize = 10_000;
const MAX_WALK_PATH_BYTES: usize = 4 * 1024 * 1024;
const INVALID_ARGUMENTS_DETAIL: &str = "invalid bounded workspace-tool arguments";
const PATH_REJECTED_DETAIL: &str = "workspace path rejected";
const FILESYSTEM_FAILED_DETAIL: &str = "workspace filesystem operation failed";
const NOT_UTF8_DETAIL: &str = "workspace file is not UTF-8";
const OFFSET_NOT_BOUNDARY_DETAIL: &str = "workspace read offset is not a character boundary";

fn default_read_max_bytes() -> usize {
    DEFAULT_READ_MAX_BYTES
}

fn default_max_results() -> usize {
    DEFAULT_MAX_RESULTS
}

fn default_path() -> String {
    String::from(".")
}

/// Typed `read_file` arguments.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadFileArguments {
    /// Relative file path inside the injected root.
    #[schemars(length(min = 1, max = crate::path::MAX_WORKSPACE_PATH_CHARACTERS))]
    pub path: String,
    /// Maximum UTF-8 content bytes retained, from 1 through 32768.
    #[serde(default = "default_read_max_bytes")]
    #[schemars(range(min = 1, max = MAX_WORKSPACE_READ_BYTES))]
    pub max_bytes: usize,
    /// Byte offset the returned content starts at, from 0. It must fall on a
    /// character boundary; continue a truncated read by passing the previous
    /// result's `next_offset`.
    #[serde(default)]
    #[schemars(range(min = 0))]
    pub offset: u64,
}

struct ReadFileContract;

impl ToolContract for ReadFileContract {
    type Arguments = ReadFileArguments;
    const NAME: &'static str = READ_FILE_NAME;
    const DESCRIPTION: &'static str = "Reads a bounded UTF-8 window of one injected-workspace \
         file; continue past it with the returned next_offset.";
}

/// Typed `list_directory` arguments.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListDirectoryArguments {
    /// Relative directory path inside the injected root.
    #[serde(default = "default_path")]
    #[schemars(length(min = 1, max = crate::path::MAX_WORKSPACE_PATH_CHARACTERS))]
    pub path: String,
    /// Maximum entries returned, from 1 through 256.
    #[serde(default = "default_max_results")]
    #[schemars(range(min = 1, max = MAX_RESULTS))]
    pub max_results: usize,
}

struct ListDirectoryContract;

impl ToolContract for ListDirectoryContract {
    type Arguments = ListDirectoryArguments;
    const NAME: &'static str = LIST_DIRECTORY_NAME;
    const DESCRIPTION: &'static str =
        "Lists bounded immediate entries in one injected-workspace directory.";
}

/// Typed `glob_files` arguments.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GlobFilesArguments {
    /// Glob pattern relative to `path`; parent traversal is forbidden.
    #[schemars(length(min = 1, max = MAX_PATTERN_CHARACTERS))]
    pub pattern: String,
    /// Relative directory from which the glob is evaluated.
    #[serde(default = "default_path")]
    #[schemars(length(min = 1, max = crate::path::MAX_WORKSPACE_PATH_CHARACTERS))]
    pub path: String,
    /// Maximum matches returned, from 1 through 256.
    #[serde(default = "default_max_results")]
    #[schemars(range(min = 1, max = MAX_RESULTS))]
    pub max_results: usize,
}

struct GlobFilesContract;

impl ToolContract for GlobFilesContract {
    type Arguments = GlobFilesArguments;
    const NAME: &'static str = GLOB_FILES_NAME;
    const DESCRIPTION: &'static str =
        "Finds bounded paths by glob without following workspace symlinks.";
}

/// Typed `search_files` arguments.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchFilesArguments {
    /// Rust regular expression matched independently against each UTF-8 line.
    #[schemars(length(min = 1, max = MAX_PATTERN_CHARACTERS))]
    pub pattern: String,
    /// Relative directory or file to search.
    #[serde(default = "default_path")]
    #[schemars(length(min = 1, max = crate::path::MAX_WORKSPACE_PATH_CHARACTERS))]
    pub path: String,
    /// Optional glob restricting relative file paths.
    #[serde(default)]
    #[schemars(length(min = 1, max = MAX_PATTERN_CHARACTERS))]
    pub glob: Option<String>,
    /// Maximum line matches returned, from 1 through 256.
    #[serde(default = "default_max_results")]
    #[schemars(range(min = 1, max = MAX_RESULTS))]
    pub max_results: usize,
}

struct SearchFilesContract;

impl ToolContract for SearchFilesContract {
    type Arguments = SearchFilesArguments;
    const NAME: &'static str = SEARCH_FILES_NAME;
    const DESCRIPTION: &'static str =
        "Searches bounded UTF-8 workspace content by regular expression.";
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadToolKind {
    ReadFile,
    ListDirectory,
    GlobFiles,
    SearchFiles,
}

impl ReadToolKind {
    const ALL: [Self; 4] = [
        Self::ReadFile,
        Self::ListDirectory,
        Self::GlobFiles,
        Self::SearchFiles,
    ];

    fn definition(self) -> Result<signalbox_application::ToolDefinition, ToolContractCompileError> {
        match self {
            Self::ReadFile => compile_contract_definition::<ReadFileContract>(
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
            Self::ListDirectory => compile_contract_definition::<ListDirectoryContract>(
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
            Self::GlobFiles => compile_contract_definition::<GlobFilesContract>(
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
            Self::SearchFiles => compile_contract_definition::<SearchFilesContract>(
                ToolPermissionDefault::Auto,
                ToolEffectClass::EffectFree,
            ),
        }
    }
}

/// A static read-family declaration or injected root could not be constructed.
#[derive(Debug)]
pub enum WorkspaceReadToolConstructionError {
    /// One static contract name was invalid.
    Name,
    /// One static contract schema was invalid.
    Schema,
    /// One static error detail was invalid.
    ErrorDetail,
    /// The catalog unexpectedly contained a duplicate.
    Duplicate,
    /// The injected root was invalid.
    Root(WorkspaceRootError),
}

impl fmt::Display for WorkspaceReadToolConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Name => "workspace read-tool static name is invalid",
            Self::Schema => "workspace read-tool static schema is invalid",
            Self::ErrorDetail => "workspace read-tool static error detail is invalid",
            Self::Duplicate => "workspace read-tool catalog is duplicated",
            Self::Root(_) => "workspace read-tool root is invalid",
        })
    }
}

impl Error for WorkspaceReadToolConstructionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Root(error) => Some(error),
            Self::Name | Self::Schema | Self::ErrorDetail | Self::Duplicate => None,
        }
    }
}

/// Compiled read-family catalog and executor around injected filesystem
/// authority.
#[derive(Clone, Debug)]
pub struct WorkspaceReadTools<FileSystem> {
    catalog: CompiledToolCatalog,
    executor: WorkspaceReadExecutor<FileSystem>,
}

impl<FileSystem: WorkspaceFileSystem> WorkspaceReadTools<FileSystem> {
    /// Compiles the four read tools around one filesystem and workspace root.
    pub fn try_new(
        filesystem: FileSystem,
        root: impl AsRef<Path>,
    ) -> Result<Self, WorkspaceReadToolConstructionError> {
        let root = WorkspaceRoot::try_new(&filesystem, root)
            .map_err(WorkspaceReadToolConstructionError::Root)?;
        let invalid_arguments_detail = detail(INVALID_ARGUMENTS_DETAIL)?;
        let path_rejected_detail = detail(PATH_REJECTED_DETAIL)?;
        let filesystem_failed_detail = detail(FILESYSTEM_FAILED_DETAIL)?;
        let not_utf8_detail = detail(NOT_UTF8_DETAIL)?;
        let offset_not_boundary_detail = detail(OFFSET_NOT_BOUNDARY_DETAIL)?;
        let compiled = ReadToolKind::ALL
            .into_iter()
            .map(|kind| {
                let definition = kind.definition().map_err(|error| match error {
                    ToolContractCompileError::Name => WorkspaceReadToolConstructionError::Name,
                    ToolContractCompileError::Schema => WorkspaceReadToolConstructionError::Schema,
                })?;
                Ok(CompiledTool::new(
                    definition,
                    WorkspaceReadArgumentValidator {
                        kind,
                        detail: invalid_arguments_detail.clone(),
                    },
                ))
            })
            .collect::<Result<Vec<_>, WorkspaceReadToolConstructionError>>()?;
        let catalog = CompiledToolCatalog::try_new(compiled)
            .map_err(|_| WorkspaceReadToolConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: WorkspaceReadExecutor {
                filesystem,
                root,
                path_rejected_detail,
                filesystem_failed_detail,
                not_utf8_detail,
                offset_not_boundary_detail,
            },
        })
    }

    /// Returns the catalog and executor as separate composition roles.
    pub fn into_parts(self) -> (CompiledToolCatalog, WorkspaceReadExecutor<FileSystem>) {
        (self.catalog, self.executor)
    }
}

fn detail(value: &str) -> Result<ToolExecutionErrorDetail, WorkspaceReadToolConstructionError> {
    ToolExecutionErrorDetail::try_new(String::from(value))
        .map_err(|_| WorkspaceReadToolConstructionError::ErrorDetail)
}

#[derive(Clone, Debug)]
struct WorkspaceReadArgumentValidator {
    kind: ReadToolKind,
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for WorkspaceReadArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_arguments(self.kind, arguments)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}

#[derive(Debug)]
enum ReadOperation {
    ReadFile {
        path: PathBuf,
        display_path: String,
        offset: u64,
        max_bytes: usize,
    },
    ListDirectory {
        path: PathBuf,
        max_results: usize,
    },
    GlobFiles {
        path: PathBuf,
        pattern: Pattern,
        max_results: usize,
    },
    SearchFiles {
        path: PathBuf,
        pattern: Regex,
        glob: Option<Pattern>,
        max_results: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InvalidReadArguments {
    Shape,
    Path(WorkspacePathRejection),
}

fn decode_operation<FileSystem: WorkspaceFileSystem>(
    kind: ReadToolKind,
    arguments: &NormalizedToolArguments,
    _filesystem: &FileSystem,
    _root: &WorkspaceRoot,
) -> Result<ReadOperation, InvalidReadArguments> {
    decode_arguments(kind, arguments)
}

fn decode_arguments(
    kind: ReadToolKind,
    arguments: &NormalizedToolArguments,
) -> Result<ReadOperation, InvalidReadArguments> {
    match kind {
        ReadToolKind::ReadFile => {
            let decoded: ReadFileArguments = serde_json::from_str(arguments.as_str())
                .map_err(|_| InvalidReadArguments::Shape)?;
            check_read_max(decoded.max_bytes)?;
            let path = checked_path(&decoded.path)?;
            Ok(ReadOperation::ReadFile {
                path,
                display_path: decoded.path,
                offset: decoded.offset,
                max_bytes: decoded.max_bytes,
            })
        }
        ReadToolKind::ListDirectory => {
            let decoded: ListDirectoryArguments = serde_json::from_str(arguments.as_str())
                .map_err(|_| InvalidReadArguments::Shape)?;
            check_max_results(decoded.max_results)?;
            let path = checked_path(&decoded.path)?;
            Ok(ReadOperation::ListDirectory {
                path,
                max_results: decoded.max_results,
            })
        }
        ReadToolKind::GlobFiles => {
            let decoded: GlobFilesArguments = serde_json::from_str(arguments.as_str())
                .map_err(|_| InvalidReadArguments::Shape)?;
            check_max_results(decoded.max_results)?;
            let pattern = checked_glob(&decoded.pattern)?;
            let path = checked_path(&decoded.path)?;
            Ok(ReadOperation::GlobFiles {
                path,
                pattern,
                max_results: decoded.max_results,
            })
        }
        ReadToolKind::SearchFiles => {
            let decoded: SearchFilesArguments = serde_json::from_str(arguments.as_str())
                .map_err(|_| InvalidReadArguments::Shape)?;
            check_max_results(decoded.max_results)?;
            if decoded.pattern.is_empty() || text_exceeds_pattern_bound(&decoded.pattern) {
                return Err(InvalidReadArguments::Shape);
            }
            let pattern = Regex::new(&decoded.pattern).map_err(|_| InvalidReadArguments::Shape)?;
            let glob = decoded.glob.as_deref().map(checked_glob).transpose()?;
            let path = checked_path(&decoded.path)?;
            Ok(ReadOperation::SearchFiles {
                path,
                pattern,
                glob,
                max_results: decoded.max_results,
            })
        }
    }
}

fn check_read_max(max_bytes: usize) -> Result<(), InvalidReadArguments> {
    if max_bytes == 0 || max_bytes > MAX_WORKSPACE_READ_BYTES {
        return Err(InvalidReadArguments::Shape);
    }
    Ok(())
}

fn check_max_results(max_results: usize) -> Result<(), InvalidReadArguments> {
    if max_results == 0 || max_results > MAX_RESULTS {
        return Err(InvalidReadArguments::Shape);
    }
    Ok(())
}

fn checked_glob(value: &str) -> Result<Pattern, InvalidReadArguments> {
    if value.is_empty() || text_exceeds_pattern_bound(value) {
        return Err(InvalidReadArguments::Shape);
    }
    validate_relative_path(value).map_err(InvalidReadArguments::Path)?;
    Pattern::new(value).map_err(|_| InvalidReadArguments::Shape)
}

fn checked_path(path: &str) -> Result<PathBuf, InvalidReadArguments> {
    validate_relative_path(path).map_err(InvalidReadArguments::Path)?;
    Ok(PathBuf::from(path))
}

fn text_exceeds_pattern_bound(value: &str) -> bool {
    value.chars().count() > MAX_PATTERN_CHARACTERS || value.len() > MAX_PATTERN_BYTES
}

/// Daemon-local executor for the four bounded workspace read tools.
#[derive(Clone, Debug)]
pub struct WorkspaceReadExecutor<FileSystem> {
    filesystem: FileSystem,
    root: WorkspaceRoot,
    path_rejected_detail: ToolExecutionErrorDetail,
    filesystem_failed_detail: ToolExecutionErrorDetail,
    not_utf8_detail: ToolExecutionErrorDetail,
    offset_not_boundary_detail: ToolExecutionErrorDetail,
}

/// A checked catalog/executor assumption failed inside the read family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceReadExecutorError {
    /// Executor argument decoding disagreed with catalog validation.
    ArgumentValidationDrift,
    /// Compact result encoding unexpectedly failed.
    ResultEncoding,
}

impl fmt::Display for WorkspaceReadExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ArgumentValidationDrift => "workspace read-tool argument validation drifted",
            Self::ResultEncoding => "workspace read-tool result encoding failed",
        })
    }
}

impl Error for WorkspaceReadExecutorError {}

impl ClassifyOperatorFailure for WorkspaceReadExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::CallerOrHubBug
    }
}

impl<FileSystem: WorkspaceFileSystem> ToolExecutor for WorkspaceReadExecutor<FileSystem> {
    type Error = WorkspaceReadExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let kind = kind_for_name(invocation.request().name().as_str())
            .ok_or(WorkspaceReadExecutorError::ArgumentValidationDrift)?;
        let operation = match decode_operation(
            kind,
            invocation.request().arguments(),
            &self.filesystem,
            &self.root,
        ) {
            Ok(operation) => operation,
            Err(InvalidReadArguments::Shape) => {
                return Err(WorkspaceReadExecutorError::ArgumentValidationDrift);
            }
            Err(InvalidReadArguments::Path(_)) => {
                return Ok(invocation.bind(ToolExecutorEvidence::KnownFailed {
                    detail: Some(self.path_rejected_detail.clone()),
                }));
            }
        };
        let evidence = match self.execute_operation(operation) {
            Ok(value) => ToolExecutorEvidence::CompletedText(encode_read_result(value)?),
            Err(ReadFailure::PathRejected) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.path_rejected_detail.clone()),
            },
            Err(ReadFailure::Filesystem) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.filesystem_failed_detail.clone()),
            },
            Err(ReadFailure::NotUtf8) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.not_utf8_detail.clone()),
            },
            Err(ReadFailure::OffsetNotOnBoundary) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.offset_not_boundary_detail.clone()),
            },
        };
        Ok(invocation.bind(evidence))
    }
}

fn map_resolve_failure(error: WorkspaceResolveError) -> ReadFailure {
    match error {
        WorkspaceResolveError::Rejected(_) => ReadFailure::PathRejected,
        WorkspaceResolveError::Io { .. } => ReadFailure::Filesystem,
    }
}

fn kind_for_name(name: &str) -> Option<ReadToolKind> {
    match name {
        READ_FILE_NAME => Some(ReadToolKind::ReadFile),
        LIST_DIRECTORY_NAME => Some(ReadToolKind::ListDirectory),
        GLOB_FILES_NAME => Some(ReadToolKind::GlobFiles),
        SEARCH_FILES_NAME => Some(ReadToolKind::SearchFiles),
        _ => None,
    }
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(untagged)]
enum ReadResult {
    ReadFile(ReadFileResult),
    ListDirectory(ListDirectoryResult),
    GlobFiles(GlobFilesResult),
    SearchFiles(SearchFilesResult),
}

impl ReadResult {
    fn evidence_units(&self) -> usize {
        match self {
            Self::ReadFile(result) => result.content.len(),
            Self::ListDirectory(result) => result.entries.len(),
            Self::GlobFiles(result) => result.matches.len(),
            Self::SearchFiles(result) => result.matches.len(),
        }
    }

    fn truncated_to(&self, units: usize) -> Self {
        match self {
            Self::ReadFile(result) => {
                let mut result = result.clone();
                let mut boundary = units.min(result.content.len());
                while !result.content.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                result.content.truncate(boundary);
                result.bytes_read = boundary;
                result.next_offset = result.offset.saturating_add(boundary as u64);
                result.truncated = true;
                Self::ReadFile(result)
            }
            Self::ListDirectory(result) => {
                let mut result = result.clone();
                result.entries.truncate(units);
                result.truncated = true;
                Self::ListDirectory(result)
            }
            Self::GlobFiles(result) => {
                let mut result = result.clone();
                result.matches.truncate(units);
                result.truncated = true;
                Self::GlobFiles(result)
            }
            Self::SearchFiles(result) => {
                let mut result = result.clone();
                result.matches.truncate(units);
                result.truncated = true;
                Self::SearchFiles(result)
            }
        }
    }
}

fn encode_admitted_result(
    result: &ReadResult,
) -> Result<Option<String>, WorkspaceReadExecutorError> {
    let encoded =
        serde_json::to_string(result).map_err(|_| WorkspaceReadExecutorError::ResultEncoding)?;
    match ToolResultText::try_new(encoded) {
        Ok(admitted) => Ok(Some(admitted.into_string())),
        Err(error) if matches!(error.failure(), ToolResultTextFailure::TooLarge { .. }) => Ok(None),
        Err(_) => Err(WorkspaceReadExecutorError::ResultEncoding),
    }
}

fn encode_read_result(result: ReadResult) -> Result<String, WorkspaceReadExecutorError> {
    if let Some(encoded) = encode_admitted_result(&result)? {
        return Ok(encoded);
    }
    let mut lower = 0_usize;
    let mut upper = result.evidence_units().saturating_sub(1);
    while lower < upper {
        let candidate_units = lower + (upper - lower).div_ceil(2);
        let candidate = result.truncated_to(candidate_units);
        if encode_admitted_result(&candidate)?.is_some() {
            lower = candidate_units;
        } else {
            upper = candidate_units - 1;
        }
    }
    let truncated = result.truncated_to(lower);
    encode_admitted_result(&truncated)?.ok_or(WorkspaceReadExecutorError::ResultEncoding)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadFailure {
    PathRejected,
    Filesystem,
    NotUtf8,
    OffsetNotOnBoundary,
}

impl<FileSystem: WorkspaceFileSystem> WorkspaceReadExecutor<FileSystem> {
    fn execute_operation(&self, operation: ReadOperation) -> Result<ReadResult, ReadFailure> {
        match operation {
            ReadOperation::ReadFile {
                path,
                display_path,
                offset,
                max_bytes,
            } => self
                .read_file(&path, display_path, offset, max_bytes)
                .map(ReadResult::ReadFile),
            ReadOperation::ListDirectory { path, max_results } => self
                .list_directory(&path, max_results)
                .map(ReadResult::ListDirectory),
            ReadOperation::GlobFiles {
                path,
                pattern,
                max_results,
            } => self
                .glob_files(&path, &pattern, max_results)
                .map(ReadResult::GlobFiles),
            ReadOperation::SearchFiles {
                path,
                pattern,
                glob,
                max_results,
            } => self
                .search_files(&path, &pattern, glob.as_ref(), max_results)
                .map(ReadResult::SearchFiles),
        }
    }

    fn read_file(
        &self,
        path: &Path,
        display_path: String,
        offset: u64,
        max_bytes: usize,
    ) -> Result<ReadFileResult, ReadFailure> {
        let read = self
            .filesystem
            .read_file_range(&self.root, path, offset, max_bytes)
            .map_err(map_resolve_failure)?;
        // The window must begin on a character boundary. Byte zero always is
        // one, so a continuation byte there is malformed file content and
        // nothing else. Past zero the two readings — a mid-character offset
        // and a malformed file — are indistinguishable without decoding from
        // the file's start, so the offset is refused on its own terms rather
        // than skipped: skipping would return content while silently hiding
        // whichever bytes the caller landed on.
        if read
            .bytes
            .first()
            .is_some_and(|byte| is_continuation_byte(*byte))
        {
            return Err(if offset == 0 {
                ReadFailure::NotUtf8
            } else {
                ReadFailure::OffsetNotOnBoundary
            });
        }
        let retained = utf8_prefix(&read.bytes, max_bytes).ok_or(ReadFailure::NotUtf8)?;
        // A window narrower than the character it begins at would retain
        // nothing and leave the cursor where it was, so following the
        // contract's own `next_offset` would repeat that page forever. The
        // page admits that one character instead, overshooting the byte bound
        // by at most three bytes, because advancing is the stronger promise.
        let retained = match retained.is_empty() && !read.bytes.is_empty() {
            true => {
                let width = character_width(read.bytes[0]).ok_or(ReadFailure::NotUtf8)?;
                utf8_prefix(&read.bytes, width).ok_or(ReadFailure::NotUtf8)?
            }
            false => retained,
        };
        let bytes_read = retained.len();
        let next_offset = offset.saturating_add(bytes_read as u64);
        Ok(ReadFileResult {
            path: display_path,
            content: retained.to_owned(),
            offset,
            bytes_read,
            next_offset,
            total_bytes: read.total_bytes,
            // The cursor decides truncation: content remains exactly when the
            // continuation offset has not reached the observed file size.
            truncated: next_offset < read.total_bytes,
        })
    }

    fn list_directory(
        &self,
        path: &Path,
        max_results: usize,
    ) -> Result<ListDirectoryResult, ReadFailure> {
        let read = self
            .filesystem
            .read_directory(
                &self.root,
                path,
                max_results,
                MAX_WALK_ENTRIES,
                MAX_WALK_PATH_BYTES,
            )
            .map_err(map_resolve_failure)?;
        let entries = read.entries;
        let truncated = read.truncated;
        let entries = entries
            .into_iter()
            .map(|entry| {
                self.root
                    .relative_name(&entry.path)
                    .map(|path| GlobMatch {
                        path,
                        kind: entry.kind,
                    })
                    .ok_or(ReadFailure::PathRejected)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ListDirectoryResult { entries, truncated })
    }

    fn glob_files(
        &self,
        path: &Path,
        pattern: &Pattern,
        max_results: usize,
    ) -> Result<GlobFilesResult, ReadFailure> {
        let walk = self.walk(path)?;
        let mut matches = walk
            .entries
            .into_iter()
            .filter_map(|entry| {
                let relative_to_base = if path == Path::new(".") {
                    entry.path.as_path()
                } else {
                    entry.path.strip_prefix(path).ok()?
                };
                pattern.matches_path(relative_to_base).then(|| {
                    self.root.relative_name(&entry.path).map(|path| GlobMatch {
                        path,
                        kind: entry.kind,
                    })
                })
            })
            .collect::<Option<Vec<_>>>()
            .ok_or(ReadFailure::PathRejected)?;
        matches.sort_by(|left, right| left.path.cmp(&right.path));
        let truncated = walk.truncated || matches.len() > max_results;
        matches.truncate(max_results);
        Ok(GlobFilesResult { matches, truncated })
    }

    fn search_files(
        &self,
        path: &Path,
        pattern: &Regex,
        glob: Option<&Pattern>,
        max_results: usize,
    ) -> Result<SearchFilesResult, ReadFailure> {
        let metadata = self
            .filesystem
            .entry_kind(&self.root, path)
            .map_err(map_resolve_failure)?;
        let single_file = metadata == WorkspaceEntryKind::File;
        let walk = if single_file {
            WalkResult {
                entries: vec![crate::WorkspaceDirectoryEntry {
                    path: path.to_owned(),
                    kind: WorkspaceEntryKind::File,
                }],
                truncated: false,
            }
        } else {
            self.walk(path)?
        };
        let mut entries = walk.entries;
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        let mut result = SearchFilesResult {
            matches: Vec::new(),
            truncated: walk.truncated,
        };
        let mut remaining_bytes = MAX_SEARCH_TOTAL_BYTES;
        for entry in entries {
            if entry.kind != WorkspaceEntryKind::File {
                continue;
            }
            let relative_to_base = if single_file {
                entry.path.file_name().map(Path::new).unwrap_or(&entry.path)
            } else if path == Path::new(".") {
                entry.path.as_path()
            } else {
                entry.path.strip_prefix(path).unwrap_or(&entry.path)
            };
            if glob.is_some_and(|filter| !filter.matches_path(relative_to_base)) {
                continue;
            }
            let max_file_bytes = if single_file {
                MAX_SEARCH_FILE_BYTES
            } else {
                if remaining_bytes <= 4 {
                    result.truncated = true;
                    break;
                }
                MAX_SEARCH_FILE_BYTES.min(remaining_bytes - 4)
            };
            let read =
                match self
                    .filesystem
                    .read_file_prefix(&self.root, &entry.path, max_file_bytes)
                {
                    Ok(read) => read,
                    Err(error) if single_file => return Err(map_resolve_failure(error)),
                    Err(_) => {
                        result.truncated = true;
                        continue;
                    }
                };
            if !single_file {
                remaining_bytes = remaining_bytes.saturating_sub(read.bytes.len());
            }
            let retained = match utf8_prefix(&read.bytes, max_file_bytes) {
                Some(retained) => retained,
                None if single_file => return Err(ReadFailure::NotUtf8),
                None => {
                    result.truncated = true;
                    continue;
                }
            };
            let evidence_truncated = read.truncated || (retained.len() as u64) < read.total_bytes;
            if evidence_truncated {
                result.truncated = true;
            }
            let complete_lines = complete_search_lines(retained, evidence_truncated);
            self.collect_matches(
                &entry.path,
                complete_lines,
                pattern,
                max_results,
                &mut result,
            )?;
            if result.matches.len() > max_results {
                result.matches.truncate(max_results);
                result.truncated = true;
                break;
            }
        }
        Ok(result)
    }

    fn collect_matches(
        &self,
        path: &Path,
        content: &str,
        pattern: &Regex,
        max_results: usize,
        result: &mut SearchFilesResult,
    ) -> Result<(), ReadFailure> {
        let display_path = self
            .root
            .relative_name(path)
            .ok_or(ReadFailure::PathRejected)?;
        for (line_index, line) in content.lines().enumerate() {
            let Some(found) = pattern.find(line) else {
                continue;
            };
            let (text, text_start_column, line_truncated) =
                bounded_match_window(line, found.start(), MAX_SEARCH_LINE_BYTES);
            result.matches.push(SearchMatch {
                path: display_path.clone(),
                line: line_index + 1,
                column: found.start() + 1,
                text_start_column,
                text: text.to_owned(),
                line_truncated,
            });
            if result.matches.len() > max_results {
                break;
            }
        }
        Ok(())
    }

    fn walk(&self, start: &Path) -> Result<WalkResult, ReadFailure> {
        self.walk_with_limits(start, MAX_WALK_ENTRIES, MAX_WALK_PATH_BYTES)
    }

    fn walk_with_limits(
        &self,
        start: &Path,
        max_entries: usize,
        max_path_bytes: usize,
    ) -> Result<WalkResult, ReadFailure> {
        let mut pending = vec![start.to_owned()];
        let mut entries = Vec::new();
        let mut visited = 0_usize;
        let mut inspected = 0_usize;
        let mut inspected_path_bytes = 0_usize;
        while let Some(directory) = pending.pop() {
            let remaining_entries = max_entries.saturating_sub(visited);
            let remaining_inspections = max_entries.saturating_sub(inspected);
            let remaining_path_bytes = max_path_bytes.saturating_sub(inspected_path_bytes);
            let read = self
                .filesystem
                .read_directory(
                    &self.root,
                    &directory,
                    remaining_entries,
                    remaining_inspections,
                    remaining_path_bytes,
                )
                .map_err(map_resolve_failure)?;
            let directory_truncated = read.truncated;
            let mut children = read.entries;
            children.sort_by(|left, right| right.path.cmp(&left.path));
            for child in children {
                visited = visited.saturating_add(1);
                if child.kind == WorkspaceEntryKind::Directory {
                    pending.push(child.path.clone());
                }
                entries.push(child);
            }
            inspected = inspected.saturating_add(read.inspected_entries);
            inspected_path_bytes = inspected_path_bytes.saturating_add(read.inspected_path_bytes);
            if directory_truncated {
                return Ok(WalkResult {
                    entries,
                    truncated: true,
                });
            }
        }
        Ok(WalkResult {
            entries,
            truncated: false,
        })
    }
}

/// Reports whether a byte continues a UTF-8 character rather than beginning
/// one, which is what a window opening mid-character starts with.
fn is_continuation_byte(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

/// Returns how many bytes the character beginning with this lead byte spans,
/// or `None` when the byte begins no character at all.
fn character_width(lead: u8) -> Option<usize> {
    match lead {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
    }
}

fn utf8_prefix(bytes: &[u8], max_bytes: usize) -> Option<&str> {
    let mut boundary = max_bytes.min(bytes.len());
    if std::str::from_utf8(bytes).is_err_and(|error| error.valid_up_to() < boundary) {
        return None;
    }
    loop {
        match std::str::from_utf8(&bytes[..boundary]) {
            Ok(value) => return Some(value),
            Err(error) if error.error_len().is_none() && boundary > 0 => boundary -= 1,
            Err(_) => return None,
        }
    }
}

fn complete_search_lines(content: &str, evidence_truncated: bool) -> &str {
    if !evidence_truncated || content.ends_with('\n') {
        return content;
    }
    content
        .rfind('\n')
        .map_or("", |last_newline| &content[..=last_newline])
}

fn bounded_match_window(value: &str, match_start: usize, max_bytes: usize) -> (&str, usize, bool) {
    if value.len() <= max_bytes {
        return (value, 1, false);
    }
    let mut start = if match_start < max_bytes {
        0
    } else {
        match_start
    };
    let mut end = start.saturating_add(max_bytes).min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    let match_end = value[match_start..]
        .chars()
        .next()
        .map_or(match_start, |character| match_start + character.len_utf8());
    if end < match_end {
        end = match_end;
        start = end.saturating_sub(max_bytes);
        while !value.is_char_boundary(start) {
            start += 1;
        }
    }
    (&value[start..end], start + 1, true)
}

struct WalkResult {
    entries: Vec<crate::WorkspaceDirectoryEntry>,
    truncated: bool,
}

/// Bounded `read_file` result.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ReadFileResult {
    /// Requested relative path.
    pub path: String,
    /// Retained UTF-8 window.
    pub content: String,
    /// Byte offset `content` begins at.
    pub offset: u64,
    /// UTF-8 bytes retained in `content`.
    pub bytes_read: usize,
    /// Offset that continues this read; pass it as the next call's `offset`.
    pub next_offset: u64,
    /// File bytes observed from the opened handle.
    pub total_bytes: u64,
    /// Whether bytes beyond `content` were omitted.
    pub truncated: bool,
}

/// One listed or globbed path.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct GlobMatch {
    /// Root-relative slash-separated path.
    pub path: String,
    /// Entry kind without following a final symlink.
    pub kind: WorkspaceEntryKind,
}

/// Bounded immediate directory listing.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ListDirectoryResult {
    /// Deterministically sorted immediate entries.
    pub entries: Vec<GlobMatch>,
    /// Whether the entry cap omitted further entries.
    pub truncated: bool,
}

/// Bounded recursive glob result.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct GlobFilesResult {
    /// Deterministically sorted matching paths.
    pub matches: Vec<GlobMatch>,
    /// Whether the result or traversal cap omitted possible matches.
    pub truncated: bool,
}

/// One content-search line match.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct SearchMatch {
    /// Root-relative slash-separated file path.
    pub path: String,
    /// One-based line number.
    pub line: usize,
    /// One-based UTF-8 byte column.
    pub column: usize,
    /// One-based UTF-8 byte column where `text` begins.
    pub text_start_column: usize,
    /// Bounded line window containing the match start.
    pub text: String,
    /// Whether the line text exceeded its output cap.
    pub line_truncated: bool,
}

/// Bounded content-search result.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct SearchFilesResult {
    /// Matches in deterministic path and line order.
    pub matches: Vec<SearchMatch>,
    /// Whether a traversal, file, or result cap omitted possible matches.
    pub truncated: bool,
}

#[cfg(test)]
mod contract_tests;

#[cfg(test)]
mod tests {
    use std::fs;

    use signalbox_application::{ToolCatalog, ToolCatalogValidationFailure};

    use super::*;
    use crate::LocalWorkspaceFileSystem;

    fn arguments(value: &str) -> NormalizedToolArguments {
        NormalizedToolArguments::try_from_provider_text(value.to_owned())
            .expect("fixture arguments are admitted")
    }

    fn fixture_tools(
        workspace: &tempfile::TempDir,
    ) -> (
        CompiledToolCatalog,
        WorkspaceReadExecutor<LocalWorkspaceFileSystem>,
    ) {
        WorkspaceReadTools::try_new(LocalWorkspaceFileSystem, workspace.path())
            .expect("fixture tools construct")
            .into_parts()
    }

    #[test]
    fn read_family_definitions_are_auto_approved_and_effect_free() {
        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        let (catalog, _executor) = fixture_tools(&workspace);
        let definitions = catalog.definitions();

        assert_eq!(definitions.len(), 4);
        assert_eq!(definitions[0].name().as_str(), GLOB_FILES_NAME);
        assert_eq!(
            definitions[0].permission_default(),
            ToolPermissionDefault::Auto
        );
        assert_eq!(definitions[0].effect_class(), ToolEffectClass::EffectFree);
        assert_eq!(definitions[1].name().as_str(), LIST_DIRECTORY_NAME);
        assert_eq!(
            definitions[1].permission_default(),
            ToolPermissionDefault::Auto
        );
        assert_eq!(definitions[1].effect_class(), ToolEffectClass::EffectFree);
        assert_eq!(definitions[2].name().as_str(), READ_FILE_NAME);
        assert_eq!(
            definitions[2].permission_default(),
            ToolPermissionDefault::Auto
        );
        assert_eq!(definitions[2].effect_class(), ToolEffectClass::EffectFree);
        assert_eq!(definitions[3].name().as_str(), SEARCH_FILES_NAME);
        assert_eq!(
            definitions[3].permission_default(),
            ToolPermissionDefault::Auto
        );
        assert_eq!(definitions[3].effect_class(), ToolEffectClass::EffectFree);
    }

    #[test]
    fn read_file_reports_exact_truncation() {
        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        fs::write(workspace.path().join("note.txt"), "abcdef").expect("file fixture writes");
        let (_catalog, executor) = fixture_tools(&workspace);
        let operation = decode_operation(
            ReadToolKind::ReadFile,
            &arguments(r#"{"max_bytes":4,"path":"note.txt"}"#),
            &executor.filesystem,
            &executor.root,
        )
        .expect("read arguments are valid");
        let ReadResult::ReadFile(result) = executor
            .execute_operation(operation)
            .expect("fixture read succeeds")
        else {
            panic!("read_file returns a read result")
        };

        assert_eq!(result.content, "abcd");
        assert_eq!(result.bytes_read, 4);
        assert_eq!(result.total_bytes, 6);
        assert!(result.truncated);
    }

    #[test]
    fn read_file_does_not_split_utf8_scalar_at_bound() {
        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        fs::write(workspace.path().join("note.txt"), "aéz").expect("file fixture writes");
        let (_catalog, executor) = fixture_tools(&workspace);
        let operation = decode_operation(
            ReadToolKind::ReadFile,
            &arguments(r#"{"max_bytes":2,"path":"note.txt"}"#),
            &executor.filesystem,
            &executor.root,
        )
        .expect("read arguments are valid");
        let ReadResult::ReadFile(result) = executor
            .execute_operation(operation)
            .expect("fixture read succeeds")
        else {
            panic!("read_file returns a read result")
        };

        assert_eq!(result.content, "a");
        assert_eq!(result.bytes_read, 1);
        assert_eq!(result.total_bytes, 4);
        assert!(result.truncated);
    }

    #[test]
    fn list_directory_cap_selects_lexically_first_entry() {
        const FIRST_PATH: &str = "a.txt";
        const LATER_PATH: &str = "b.txt";

        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        fs::write(workspace.path().join(LATER_PATH), "b").expect("fixture file writes");
        fs::write(workspace.path().join(FIRST_PATH), "a").expect("fixture file writes");
        let (_catalog, executor) = fixture_tools(&workspace);
        let operation = decode_operation(
            ReadToolKind::ListDirectory,
            &arguments(r#"{"max_results":1,"path":"."}"#),
            &executor.filesystem,
            &executor.root,
        )
        .expect("list arguments are valid");
        let ReadResult::ListDirectory(result) = executor
            .execute_operation(operation)
            .expect("fixture listing succeeds")
        else {
            panic!("list_directory returns a listing")
        };

        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].path, FIRST_PATH);
        assert!(result.truncated);
    }

    #[test]
    fn glob_files_matches_relative_pattern() {
        const MATCH_PATH: &str = "src/lib.rs";

        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        fs::create_dir(workspace.path().join("src")).expect("fixture directory creates");
        fs::write(workspace.path().join(MATCH_PATH), "pub fn fixture() {}")
            .expect("fixture file writes");
        fs::write(workspace.path().join("README.md"), "fixture").expect("fixture file writes");
        let (_catalog, executor) = fixture_tools(&workspace);
        let operation = decode_operation(
            ReadToolKind::GlobFiles,
            &arguments(r#"{"pattern":"**/*.rs","path":"."}"#),
            &executor.filesystem,
            &executor.root,
        )
        .expect("glob arguments are valid");
        let ReadResult::GlobFiles(result) = executor
            .execute_operation(operation)
            .expect("fixture glob succeeds")
        else {
            panic!("glob_files returns glob matches")
        };

        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].path, MATCH_PATH);
        assert!(!result.truncated);
    }

    #[test]
    fn search_files_returns_bounded_line_evidence() {
        const FILE_PATH: &str = "one.rs";

        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        fs::write(
            workspace.path().join(FILE_PATH),
            "fn first() {}\nfn second() {}\n",
        )
        .expect("fixture file writes");
        let (_catalog, executor) = fixture_tools(&workspace);
        let operation = decode_operation(
            ReadToolKind::SearchFiles,
            &arguments(r#"{"max_results":1,"path":".","pattern":"fn "}"#),
            &executor.filesystem,
            &executor.root,
        )
        .expect("search arguments are valid");
        let ReadResult::SearchFiles(result) = executor
            .execute_operation(operation)
            .expect("fixture search succeeds")
        else {
            panic!("search_files returns search matches")
        };

        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].path, FILE_PATH);
        assert_eq!(result.matches[0].line, 1);
        assert_eq!(result.matches[0].column, 1);
        assert_eq!(result.matches[0].text, "fn first() {}");
        assert!(result.truncated);
    }

    #[test]
    fn catalog_rejects_absolute_read_path() {
        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        let (catalog, _executor) = fixture_tools(&workspace);
        let name = signalbox_domain::ToolName::try_new(String::from(READ_FILE_NAME))
            .expect("fixture name is valid");
        let result = catalog.validate_arguments(&name, &arguments(r#"{"path":"/etc/passwd"}"#));
        let Err(ToolCatalogValidationFailure::InvalidArguments { .. }) = result else {
            panic!("absolute read path is rejected as invalid arguments")
        };
    }

    #[test]
    fn catalog_accepts_missing_read_path_without_io() {
        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        let (catalog, _executor) = fixture_tools(&workspace);
        let name = signalbox_domain::ToolName::try_new(String::from(READ_FILE_NAME))
            .expect("fixture name is valid");

        let result = catalog.validate_arguments(&name, &arguments(r#"{"path":"missing.txt"}"#));

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn missing_read_path_fails_at_execution() {
        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        let (_catalog, executor) = fixture_tools(&workspace);
        let operation = decode_operation(
            ReadToolKind::ReadFile,
            &arguments(r#"{"path":"missing.txt"}"#),
            &executor.filesystem,
            &executor.root,
        )
        .expect("missing path is a valid request");

        let result = executor.execute_operation(operation);
        let Err(error) = result else {
            panic!("missing path fails during execution")
        };

        assert_eq!(error, ReadFailure::Filesystem);
    }

    #[cfg(unix)]
    #[test]
    fn catalog_validation_does_not_resolve_read_symlink() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        let outside = tempfile::tempdir().expect("outside fixture constructs");
        fs::write(outside.path().join("secret.txt"), "secret").expect("outside fixture writes");
        symlink(
            outside.path().join("secret.txt"),
            workspace.path().join("escape"),
        )
        .expect("escape symlink fixture constructs");
        let (catalog, _executor) = fixture_tools(&workspace);
        let name = signalbox_domain::ToolName::try_new(String::from(READ_FILE_NAME))
            .expect("fixture name is valid");
        let result = catalog.validate_arguments(&name, &arguments(r#"{"path":"escape"}"#));

        assert_eq!(result, Ok(()));
    }

    #[cfg(unix)]
    #[test]
    fn catalog_validation_does_not_resolve_search_symlink() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        let outside = tempfile::tempdir().expect("outside fixture constructs");
        fs::write(outside.path().join("secret.txt"), "secret").expect("outside fixture writes");
        symlink(outside.path(), workspace.path().join("escape"))
            .expect("escape symlink fixture constructs");
        let (catalog, _executor) = fixture_tools(&workspace);
        let name = signalbox_domain::ToolName::try_new(String::from(SEARCH_FILES_NAME))
            .expect("fixture name is valid");
        let result = catalog
            .validate_arguments(&name, &arguments(r#"{"path":"escape","pattern":"secret"}"#));

        assert_eq!(result, Ok(()));
    }

    #[cfg(unix)]
    #[test]
    fn glob_recursive_traversal_never_follows_directory_symlink() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        let outside = tempfile::tempdir().expect("outside fixture constructs");
        fs::write(outside.path().join("secret.txt"), "secret").expect("outside fixture writes");
        symlink(outside.path(), workspace.path().join("linked"))
            .expect("directory symlink fixture constructs");
        let (_catalog, executor) = fixture_tools(&workspace);
        let operation = decode_operation(
            ReadToolKind::GlobFiles,
            &arguments(r#"{"pattern":"**/*.txt","path":"."}"#),
            &executor.filesystem,
            &executor.root,
        )
        .expect("glob arguments are valid");
        let ReadResult::GlobFiles(result) = executor
            .execute_operation(operation)
            .expect("fixture glob succeeds")
        else {
            panic!("glob_files returns glob matches")
        };

        assert!(result.matches.is_empty());
        assert!(!result.truncated);
    }
}
