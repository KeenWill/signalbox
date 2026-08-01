use serde::Serialize;
use signalbox_domain::ToolResultText;

use crate::failure::LocalGitFailure;

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(super) enum LocalGitResult {
    Status(StatusResult),
    Diff(DiffResult),
    Log(LogResult),
    Stage(StageResult),
    Commit(CommitResult),
    BranchCreate(BranchResult),
    BranchSwitch(BranchResult),
}

#[derive(Debug, Serialize)]
pub(super) struct StatusResult {
    branch: Option<String>,
    branch_truncated: bool,
    head: Option<String>,
    entries: Vec<StatusEntry>,
    truncated: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct StatusEntry {
    path: String,
    previous_path: Option<String>,
    index: &'static str,
    worktree: &'static str,
}

#[derive(Debug, Serialize)]
pub(super) struct DiffResult {
    patch: String,
    truncated: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct LogResult {
    commits: Vec<LogEntry>,
    truncated: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct LogEntry {
    commit: String,
    author_name: String,
    author_name_truncated: bool,
    author_email: String,
    author_email_truncated: bool,
    message: String,
    message_truncated: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct StageResult {
    pub(super) staged_paths: usize,
}

#[derive(Debug, Serialize)]
pub(super) struct CommitResult {
    pub(super) commit: String,
    pub(super) state_cleaned: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct BranchResult {
    branch: String,
    head: String,
}

pub(super) fn encode_result(result: &LocalGitResult) -> Result<String, LocalGitFailure> {
    let encoded = serde_json::to_string(result).map_err(|_| LocalGitFailure::Encoding)?;
    ToolResultText::try_new(encoded)
        .map(ToolResultText::into_string)
        .map_err(|_| LocalGitFailure::Encoding)
}
