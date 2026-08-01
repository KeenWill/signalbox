pub(super) const MAX_BRANCH_BYTES: usize = 255;

pub(super) const MAX_REVISION_BYTES: usize = 1024;

pub(super) const MAX_COMMIT_MESSAGE_BYTES: usize = 64 * 1024;

pub(super) const MAX_IDENTITY_BYTES: usize = 256;

pub(super) const MAX_STAGE_PATHS: usize = 256;

pub(super) const MAX_STAGE_FILE_BYTES: usize = MAX_OBJECT_BYTES;

pub(super) const MAX_STAGE_TOTAL_BYTES: usize = 16 * 1024 * 1024;

pub(super) const MAX_WORKTREE_TOTAL_BYTES: usize = 16 * 1024 * 1024;

pub(super) const MAX_REPOSITORY_CONFIG_BYTES: usize = 1024 * 1024;

pub(super) const MAX_PACKED_REFS_BYTES: usize = 1024 * 1024;

pub(super) const MAX_SHALLOW_ENTRIES: usize = 1024;

pub(super) const MAX_SHALLOW_BYTES: usize = MAX_SHALLOW_ENTRIES * 41;

pub(super) const MAX_INDEX_BYTES: usize = 64 * 1024 * 1024;

pub(super) const MAX_INDEX_ENTRIES: usize = MAX_WORKTREE_INSPECTIONS;

pub(super) const MAX_OBJECT_BYTES: usize = 1024 * 1024;

pub(super) const MAX_PACK_FILE_BYTES: usize = MAX_OBJECT_DATABASE_BYTES;

pub(super) const MAX_OBJECT_DATABASE_BYTES: usize = 128 * MAX_OBJECT_BYTES;

pub(super) const MAX_TREE_BLOB_BYTES: usize = 64 * MAX_OBJECT_BYTES;

pub(super) const MAX_REFLOG_BYTES: usize = 64 * MAX_OBJECT_BYTES;

pub(super) const MAX_WORKTREE_INSPECTIONS: usize = 4096;

pub(super) const MAX_MERGE_PARENTS: usize = 64;

pub(super) const MAX_MERGE_HEAD_BYTES: usize = MAX_MERGE_PARENTS * 41;

pub(super) const MAX_WORKTREE_PATH_BYTES: usize = 4 * 1024 * 1024;

pub(super) const MAX_STATUS_ENTRIES: usize = 128;

pub(super) const MAX_STATUS_PATH_BYTES: usize = 1024;

pub(super) const MAX_LOG_ENTRIES: usize = 50;

pub(super) const DEFAULT_LOG_ENTRIES: usize = 25;

pub(super) const MAX_LOG_IDENTITY_BYTES: usize = 256;

pub(super) const MAX_LOG_MESSAGE_BYTES: usize = 2048;

pub(super) const MAX_DIFF_BYTES: usize = 128 * 1024;

pub(super) const GITLINK_MODE: u32 = 0o160000;

pub(super) const INDEX_ASSUME_VALID: u16 = 1 << 15;

pub(super) const INDEX_SKIP_WORKTREE: u16 = 1 << 14;
