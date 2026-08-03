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
    pub(super) branch: Option<String>,
    pub(super) branch_truncated: bool,
    pub(super) head: Option<String>,
    pub(super) entries: Vec<StatusEntry>,
    pub(super) truncated: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct StatusEntry {
    pub(super) path: String,
    pub(super) previous_path: Option<String>,
    pub(super) index: &'static str,
    pub(super) worktree: &'static str,
}

#[derive(Debug, Serialize)]
pub(super) struct DiffResult {
    pub(super) patch: String,
    pub(super) truncated: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct LogResult {
    pub(super) commits: Vec<LogEntry>,
    pub(super) truncated: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct LogEntry {
    pub(super) commit: String,
    pub(super) author_name: String,
    pub(super) author_name_truncated: bool,
    pub(super) author_email: String,
    pub(super) author_email_truncated: bool,
    pub(super) message: String,
    pub(super) message_truncated: bool,
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
    pub(super) branch: String,
    pub(super) head: String,
}

pub(super) fn encode_result(result: &LocalGitResult) -> Result<String, LocalGitFailure> {
    let encoded = serde_json::to_string(result).map_err(|_| LocalGitFailure::Encoding)?;
    ToolResultText::try_new(encoded)
        .map(ToolResultText::into_string)
        .map_err(|_| LocalGitFailure::Encoding)
}
