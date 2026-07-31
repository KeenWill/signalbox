//! Bounded tools for an injected local workspace.

mod patch;
mod path;
mod read;

pub use patch::{
    ExpectedPatchSyntax, MAX_PATCH_BYTES, MAX_PATCH_HUNKS, MAX_PATCH_OPERATIONS,
    MalformedPatchReason, PatchApplyError, PatchApplyErrorKind, PatchHunk, PatchLocation,
    PatchOperation, PatchParseError, PatchParseErrorKind, PatchPathRejection, PatchPlan,
    PlannedPatchOperation, WorkspacePatch, apply_patch_to_contents, parse_patch, plan_patch,
};

pub use path::{
    LocalWorkspaceFileSystem, WorkspaceDirectoryEntry, WorkspaceDirectoryRead, WorkspaceEntryKind,
    WorkspaceFileBytes, WorkspaceFileSystem, WorkspacePathRejection, WorkspaceResolveError,
    WorkspaceRoot, WorkspaceRootError,
};
pub use read::{
    GLOB_FILES_NAME, GlobFilesArguments, GlobFilesResult, GlobMatch, LIST_DIRECTORY_NAME,
    ListDirectoryArguments, ListDirectoryResult, READ_FILE_NAME, ReadFileArguments, ReadFileResult,
    SEARCH_FILES_NAME, SearchFilesArguments, SearchFilesResult, SearchMatch,
    WORKSPACE_READ_TOOL_NAMES, WorkspaceReadExecutor, WorkspaceReadExecutorError,
    WorkspaceReadToolConstructionError, WorkspaceReadTools,
};
