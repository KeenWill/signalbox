use std::{
    error::Error,
    fmt,
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, de::Error as _};

use crate::limits::{
    DEFAULT_LOG_ENTRIES, MAX_BRANCH_BYTES, MAX_COMMIT_MESSAGE_BYTES, MAX_LOG_ENTRIES,
    MAX_REVISION_BYTES, MAX_STAGE_PATHS,
};

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

pub(super) fn default_log_revision() -> String {
    "HEAD".to_owned()
}

pub(super) fn default_log_entries() -> usize {
    DEFAULT_LOG_ENTRIES
}

/// Bounded history arguments.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitLogArguments {
    /// Revision expression at which traversal starts.
    #[serde(default = "default_log_revision")]
    #[schemars(length(min = 1, max = MAX_REVISION_BYTES))]
    pub(super) revision: String,
    /// Maximum commits returned.
    #[serde(
        default = "default_log_entries",
        deserialize_with = "deserialize_log_entries"
    )]
    #[schemars(range(min = 1, max = MAX_LOG_ENTRIES))]
    pub(super) max_entries: usize,
}

fn deserialize_log_entries<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    let max_entries = usize::deserialize(deserializer)?;
    if !(1..=MAX_LOG_ENTRIES).contains(&max_entries) {
        return Err(D::Error::custom("invalid bounded log entry count"));
    }
    Ok(max_entries)
}

/// Exact root-relative paths to stage.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitStageArguments {
    /// Files to add, update, or remove from the index.
    #[serde(deserialize_with = "deserialize_stage_paths")]
    #[schemars(length(min = 1, max = MAX_STAGE_PATHS))]
    pub(super) paths: Vec<String>,
}

fn deserialize_stage_paths<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let paths = Vec::<String>::deserialize(deserializer)?;
    if paths.is_empty() || paths.len() > MAX_STAGE_PATHS {
        return Err(D::Error::custom("invalid bounded stage path count"));
    }
    Ok(paths)
}

/// Verbatim commit-message arguments.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitCommitArguments {
    /// Exact commit message, interpreted only as UTF-8 data.
    #[serde(deserialize_with = "deserialize_commit_message")]
    #[schemars(length(min = 1, max = MAX_COMMIT_MESSAGE_BYTES))]
    pub(super) message: String,
}

fn deserialize_commit_message<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let message = String::deserialize(deserializer)?;
    if message.is_empty()
        || message.len() > MAX_COMMIT_MESSAGE_BYTES
        || message.as_bytes().contains(&0)
    {
        return Err(D::Error::custom("invalid bounded commit message"));
    }
    Ok(message)
}

/// Local branch creation arguments.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitBranchCreateArguments {
    /// New local branch shorthand.
    #[schemars(length(min = 1, max = MAX_BRANCH_BYTES))]
    pub(super) name: String,
    /// Revision resolving to the branch's initial commit.
    #[schemars(length(min = 1, max = MAX_REVISION_BYTES))]
    pub(super) start: String,
}

/// Local branch switch arguments.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitBranchSwitchArguments {
    /// Existing local branch shorthand.
    #[schemars(length(min = 1, max = MAX_BRANCH_BYTES))]
    pub(super) name: String,
}

#[derive(Debug)]
pub(super) enum LocalOperation {
    BranchCreate(GitBranchCreateArguments),
    BranchSwitch(GitBranchSwitchArguments),
    Commit(GitCommitArguments),
    Diff(GitDiffArguments),
    Log(GitLogArguments),
    Stage(GitStageArguments),
    Status,
}

pub(super) fn checked_relative_path(value: &str) -> Result<PathBuf, InvalidGitArguments> {
    if value.is_empty() || value.len() > 4096 || value.contains('\0') {
        return Err(InvalidGitArguments);
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return Err(InvalidGitArguments);
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(component) => normalized.push(component),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(InvalidGitArguments);
            }
        }
    }
    let first = normalized.components().next().ok_or(InvalidGitArguments)?;
    if is_repository_administration_component(first) {
        return Err(InvalidGitArguments);
    }
    Ok(normalized)
}

pub(super) fn is_repository_administration_component(component: Component<'_>) -> bool {
    matches!(
        component,
        Component::Normal(name) if name.as_bytes().eq_ignore_ascii_case(b".git")
    )
}

pub(super) fn parse_gitdir_marker(directory: &Path, bytes: &[u8]) -> Option<PathBuf> {
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
