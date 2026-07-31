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

const DEFAULT_READ_MAX_BYTES: usize = 64 * 1024;
const MAX_READ_BYTES: usize = 256 * 1024;
const DEFAULT_MAX_RESULTS: usize = 100;
const MAX_RESULTS: usize = 256;
const MAX_PATTERN_BYTES: usize = 4096;
const MAX_SEARCH_FILE_BYTES: usize = 1024 * 1024;
const MAX_SEARCH_LINE_BYTES: usize = 1024;
const MAX_WALK_ENTRIES: usize = 10_000;
const INVALID_ARGUMENTS_DETAIL: &str = "invalid bounded workspace-tool arguments";
const PATH_REJECTED_DETAIL: &str = "workspace path rejected";
const FILESYSTEM_FAILED_DETAIL: &str = "workspace filesystem operation failed";
const NOT_UTF8_DETAIL: &str = "workspace file is not UTF-8";

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
    pub path: String,
    /// Maximum UTF-8 content bytes retained, from 1 through 262144.
    #[serde(default = "default_read_max_bytes")]
    pub max_bytes: usize,
}

struct ReadFileContract;

impl ToolContract for ReadFileContract {
    type Arguments = ReadFileArguments;
    const NAME: &'static str = READ_FILE_NAME;
    const DESCRIPTION: &'static str =
        "Reads a bounded UTF-8 file prefix from the injected workspace root.";
}

/// Typed `list_directory` arguments.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListDirectoryArguments {
    /// Relative directory path inside the injected root.
    #[serde(default = "default_path")]
    pub path: String,
    /// Maximum entries returned, from 1 through 256.
    #[serde(default = "default_max_results")]
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
    pub pattern: String,
    /// Relative directory from which the glob is evaluated.
    #[serde(default = "default_path")]
    pub path: String,
    /// Maximum matches returned, from 1 through 256.
    #[serde(default = "default_max_results")]
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
    pub pattern: String,
    /// Relative directory or file to search.
    #[serde(default = "default_path")]
    pub path: String,
    /// Optional glob restricting relative file paths.
    #[serde(default)]
    pub glob: Option<String>,
    /// Maximum line matches returned, from 1 through 256.
    #[serde(default = "default_max_results")]
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
                        filesystem: filesystem.clone(),
                        root: root.clone(),
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
struct WorkspaceReadArgumentValidator<FileSystem> {
    kind: ReadToolKind,
    filesystem: FileSystem,
    root: WorkspaceRoot,
    detail: ToolExecutionErrorDetail,
}

impl<FileSystem: WorkspaceFileSystem> ToolArgumentValidator
    for WorkspaceReadArgumentValidator<FileSystem>
{
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_operation(self.kind, arguments, &self.filesystem, &self.root)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}

#[derive(Debug)]
enum ReadOperation {
    ReadFile {
        path: PathBuf,
        display_path: String,
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
    Filesystem,
}

fn decode_operation<FileSystem: WorkspaceFileSystem>(
    kind: ReadToolKind,
    arguments: &NormalizedToolArguments,
    filesystem: &FileSystem,
    root: &WorkspaceRoot,
) -> Result<ReadOperation, InvalidReadArguments> {
    match kind {
        ReadToolKind::ReadFile => {
            let decoded: ReadFileArguments = serde_json::from_str(arguments.as_str())
                .map_err(|_| InvalidReadArguments::Shape)?;
            check_read_max(decoded.max_bytes)?;
            let path = resolve(root, filesystem, &decoded.path)?;
            Ok(ReadOperation::ReadFile {
                path,
                display_path: decoded.path,
                max_bytes: decoded.max_bytes,
            })
        }
        ReadToolKind::ListDirectory => {
            let decoded: ListDirectoryArguments = serde_json::from_str(arguments.as_str())
                .map_err(|_| InvalidReadArguments::Shape)?;
            check_max_results(decoded.max_results)?;
            let path = resolve(root, filesystem, &decoded.path)?;
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
            let path = resolve(root, filesystem, &decoded.path)?;
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
            if decoded.pattern.is_empty() || decoded.pattern.len() > MAX_PATTERN_BYTES {
                return Err(InvalidReadArguments::Shape);
            }
            let pattern = Regex::new(&decoded.pattern).map_err(|_| InvalidReadArguments::Shape)?;
            let glob = decoded.glob.as_deref().map(checked_glob).transpose()?;
            let path = resolve(root, filesystem, &decoded.path)?;
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
    if max_bytes == 0 || max_bytes > MAX_READ_BYTES {
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
    if value.is_empty() || value.len() > MAX_PATTERN_BYTES {
        return Err(InvalidReadArguments::Shape);
    }
    validate_relative_path(value).map_err(InvalidReadArguments::Path)?;
    Pattern::new(value).map_err(|_| InvalidReadArguments::Shape)
}

fn resolve<FileSystem: WorkspaceFileSystem>(
    root: &WorkspaceRoot,
    filesystem: &FileSystem,
    path: &str,
) -> Result<PathBuf, InvalidReadArguments> {
    root.resolve_existing(filesystem, path)
        .map_err(|error| match error {
            WorkspaceResolveError::Rejected(reason) => InvalidReadArguments::Path(reason),
            WorkspaceResolveError::Io(_) => InvalidReadArguments::Filesystem,
        })
}

/// Daemon-local executor for the four bounded workspace read tools.
#[derive(Clone, Debug)]
pub struct WorkspaceReadExecutor<FileSystem> {
    filesystem: FileSystem,
    root: WorkspaceRoot,
    path_rejected_detail: ToolExecutionErrorDetail,
    filesystem_failed_detail: ToolExecutionErrorDetail,
    not_utf8_detail: ToolExecutionErrorDetail,
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
        let operation = decode_operation(
            kind,
            invocation.request().arguments(),
            &self.filesystem,
            &self.root,
        )
        .map_err(|_| WorkspaceReadExecutorError::ArgumentValidationDrift)?;
        let evidence = match self.execute_operation(operation) {
            Ok(value) => ToolExecutorEvidence::CompletedText(
                serde_json::to_string(&value)
                    .map_err(|_| WorkspaceReadExecutorError::ResultEncoding)?,
            ),
            Err(ReadFailure::PathRejected) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.path_rejected_detail.clone()),
            },
            Err(ReadFailure::Filesystem) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.filesystem_failed_detail.clone()),
            },
            Err(ReadFailure::NotUtf8) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.not_utf8_detail.clone()),
            },
        };
        Ok(invocation.bind(evidence))
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

#[derive(Debug, serde::Serialize)]
#[serde(untagged)]
enum ReadResult {
    ReadFile(ReadFileResult),
    ListDirectory(ListDirectoryResult),
    GlobFiles(GlobFilesResult),
    SearchFiles(SearchFilesResult),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadFailure {
    PathRejected,
    Filesystem,
    NotUtf8,
}

impl<FileSystem: WorkspaceFileSystem> WorkspaceReadExecutor<FileSystem> {
    fn execute_operation(&self, operation: ReadOperation) -> Result<ReadResult, ReadFailure> {
        match operation {
            ReadOperation::ReadFile {
                path,
                display_path,
                max_bytes,
            } => self
                .read_file(&path, display_path, max_bytes)
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
        max_bytes: usize,
    ) -> Result<ReadFileResult, ReadFailure> {
        let read = self
            .filesystem
            .read_file_prefix(path, max_bytes)
            .map_err(|_| ReadFailure::Filesystem)?;
        let retained = utf8_prefix(&read.bytes, max_bytes).ok_or(ReadFailure::NotUtf8)?;
        let bytes_read = retained.len();
        Ok(ReadFileResult {
            path: display_path,
            content: retained.to_owned(),
            bytes_read,
            total_bytes: read.total_bytes,
            truncated: (bytes_read as u64) < read.total_bytes,
        })
    }

    fn list_directory(
        &self,
        path: &Path,
        max_results: usize,
    ) -> Result<ListDirectoryResult, ReadFailure> {
        let mut entries = self
            .filesystem
            .read_directory(path)
            .map_err(|_| ReadFailure::Filesystem)?;
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        let truncated = entries.len() > max_results;
        entries.truncate(max_results);
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
                let relative_to_base = entry.path.strip_prefix(path).ok()?;
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
            .symlink_metadata(path)
            .map_err(|_| ReadFailure::Filesystem)?;
        let walk = if metadata.is_file() {
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
        let mut result = SearchFilesResult {
            matches: Vec::new(),
            truncated: walk.truncated,
        };
        for entry in walk.entries {
            if entry.kind != WorkspaceEntryKind::File {
                continue;
            }
            let relative_to_base = entry.path.strip_prefix(path).unwrap_or(&entry.path);
            if glob.is_some_and(|filter| !filter.matches_path(relative_to_base)) {
                continue;
            }
            let read = self
                .filesystem
                .read_file_prefix(&entry.path, MAX_SEARCH_FILE_BYTES)
                .map_err(|_| ReadFailure::Filesystem)?;
            let retained =
                utf8_prefix(&read.bytes, MAX_SEARCH_FILE_BYTES).ok_or(ReadFailure::NotUtf8)?;
            if (retained.len() as u64) < read.total_bytes {
                result.truncated = true;
            }
            self.collect_matches(&entry.path, retained, pattern, max_results, &mut result)?;
            if result.matches.len() == max_results {
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
            let (text, line_truncated) = bounded_utf8(line, MAX_SEARCH_LINE_BYTES);
            result.matches.push(SearchMatch {
                path: display_path.clone(),
                line: line_index + 1,
                column: found.start() + 1,
                text: text.to_owned(),
                line_truncated,
            });
            if result.matches.len() == max_results {
                break;
            }
        }
        Ok(())
    }

    fn walk(&self, start: &Path) -> Result<WalkResult, ReadFailure> {
        let mut pending = vec![start.to_owned()];
        let mut entries = Vec::new();
        let mut visited = 0_usize;
        while let Some(directory) = pending.pop() {
            let mut children = self
                .filesystem
                .read_directory(&directory)
                .map_err(|_| ReadFailure::Filesystem)?;
            children.sort_by(|left, right| right.path.cmp(&left.path));
            for child in children {
                visited += 1;
                if visited > MAX_WALK_ENTRIES {
                    return Ok(WalkResult {
                        entries,
                        truncated: true,
                    });
                }
                if child.kind == WorkspaceEntryKind::Directory {
                    pending.push(child.path.clone());
                }
                entries.push(child);
            }
        }
        Ok(WalkResult {
            entries,
            truncated: false,
        })
    }
}

fn utf8_prefix(bytes: &[u8], max_bytes: usize) -> Option<&str> {
    let mut boundary = bytes.len().min(max_bytes);
    loop {
        match std::str::from_utf8(&bytes[..boundary]) {
            Ok(value) => return Some(value),
            Err(error) if error.error_len().is_none() && boundary > 0 => boundary -= 1,
            Err(_) => return None,
        }
    }
}

fn bounded_utf8(value: &str, max_bytes: usize) -> (&str, bool) {
    if value.len() <= max_bytes {
        return (value, false);
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    (&value[..boundary], true)
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
    /// Retained UTF-8 prefix.
    pub content: String,
    /// UTF-8 bytes retained in `content`.
    pub bytes_read: usize,
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
    /// Bounded matching line text.
    pub text: String,
    /// Whether the line text exceeded its output cap.
    pub line_truncated: bool,
}

/// Bounded content-search result.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct SearchFilesResult {
    /// Matches in deterministic path and line order.
    pub matches: Vec<SearchMatch>,
    /// Whether any traversal, file, line-match, or output cap omitted content.
    pub truncated: bool,
}

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
    fn list_directory_is_sorted_and_bounded() {
        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        fs::write(workspace.path().join("b.txt"), "b").expect("fixture file writes");
        fs::write(workspace.path().join("a.txt"), "a").expect("fixture file writes");
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

        assert_eq!(result.entries[0].path, "a.txt");
        assert!(result.truncated);
    }

    #[test]
    fn glob_files_matches_relative_pattern_without_following_symlinks() {
        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        fs::create_dir(workspace.path().join("src")).expect("fixture directory creates");
        fs::write(workspace.path().join("src/lib.rs"), "pub fn fixture() {}")
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
        assert_eq!(result.matches[0].path, "src/lib.rs");
        assert!(!result.truncated);
    }

    #[test]
    fn search_files_returns_bounded_line_evidence() {
        let workspace = tempfile::tempdir().expect("workspace fixture constructs");
        fs::write(
            workspace.path().join("one.rs"),
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
        assert_eq!(result.matches[0].path, "one.rs");
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

    #[cfg(unix)]
    #[test]
    fn catalog_rejects_read_through_escaping_symlink() {
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
        let Err(ToolCatalogValidationFailure::InvalidArguments { .. }) = result else {
            panic!("escaping read symlink is rejected as invalid arguments")
        };
    }

    #[cfg(unix)]
    #[test]
    fn catalog_rejects_search_root_through_escaping_symlink() {
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
        let Err(ToolCatalogValidationFailure::InvalidArguments { .. }) = result else {
            panic!("escaping search symlink is rejected as invalid arguments")
        };
    }
}
