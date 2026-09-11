pub(super) const MAX_BRANCH_BYTES: usize = 255;

pub(super) const MAX_REFERENCE_BYTES: usize = 1024;

pub(super) const MAX_REVISION_BYTES: usize = MAX_REFERENCE_BYTES + "ref: \n".len();

pub(super) const MAX_COMMIT_MESSAGE_BYTES: usize = 64 * 1024;

pub(super) const MAX_IDENTITY_BYTES: usize = 256;

pub(super) const MAX_STAGE_PATHS: usize = 256;

pub(super) const MAX_REPOSITORY_CONFIG_BYTES: usize = 1024 * 1024;

pub(super) const MAX_PACKED_REFS_BYTES: usize = 1024 * 1024;

pub(super) const MAX_SHALLOW_ENTRIES: usize = 1024;

pub(super) const MAX_SHALLOW_BYTES: usize = MAX_SHALLOW_ENTRIES * 65;

pub(super) const MAX_INDEX_BYTES: usize = 64 * 1024 * 1024;

pub(super) const MAX_INDEX_ENTRIES: usize = MAX_WORKTREE_INSPECTIONS;

pub(super) const MAX_LOOSE_OBJECT_HEADER_BYTES: usize = 128;

pub(super) const MAX_REPOSITORY_INSPECTIONS: usize = 100_000;

pub(super) const MAX_REFLOG_BYTES: usize = 64 * 1024 * 1024;

pub(super) const MAX_WORKTREE_INSPECTIONS: usize = 4096;

pub(super) const MAX_MERGE_PARENTS: usize = 64;

pub(super) const MAX_MERGE_HEAD_BYTES: usize = MAX_MERGE_PARENTS * 65;

pub(super) const MAX_WORKTREE_PATH_BYTES: usize = 4 * 1024 * 1024;

/// Maximum number of status entries returned by one tool call.
pub const MAX_STATUS_ENTRIES: usize = 128;

pub(super) const MAX_STATUS_PATH_BYTES: usize = 1024;

pub(super) const MAX_LOG_ENTRIES: usize = 50;

pub(super) const DEFAULT_LOG_ENTRIES: usize = 25;

pub(super) const MAX_LOG_IDENTITY_BYTES: usize = 256;

pub(super) const MAX_LOG_MESSAGE_BYTES: usize = 2048;

/// Maximum number of diff bytes returned by one tool call.
pub const MAX_DIFF_BYTES: usize = 128 * 1024;

pub(super) const GITLINK_MODE: u32 = 0o160000;

pub(super) const INDEX_ASSUME_VALID: u16 = 1 << 15;

pub(super) const INDEX_SKIP_WORKTREE: u16 = 1 << 14;

// libgit2 materializes metadata objects; blob contents use streamed I/O.
pub(super) const MAX_METADATA_OBJECT_BYTES: usize = 1024 * 1024;

pub(super) fn object_byte_limit(configured: Option<usize>, kind: git2::ObjectType) -> usize {
    let configured = configured.unwrap_or(usize::MAX);
    if kind == git2::ObjectType::Blob {
        configured
    } else {
        configured.min(MAX_METADATA_OBJECT_BYTES)
    }
}
