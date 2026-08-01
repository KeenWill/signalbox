use std::collections::HashSet;

use signalbox_application::ToolArgumentValidator;
use signalbox_domain::{NormalizedToolArguments, ToolExecutionErrorDetail};

use crate::arguments::{
    GitDiffArguments, GitStatusArguments, InvalidGitArguments, LocalOperation,
    checked_relative_path,
};
use crate::contracts::LocalToolKind;
use crate::limits::{
    MAX_BRANCH_BYTES, MAX_COMMIT_MESSAGE_BYTES, MAX_LOG_ENTRIES, MAX_REVISION_BYTES,
    MAX_STAGE_PATHS,
};

pub(super) fn decode_operation(
    kind: LocalToolKind,
    arguments: &NormalizedToolArguments,
) -> Result<LocalOperation, InvalidGitArguments> {
    let operation = match kind {
        LocalToolKind::BranchCreate => {
            serde_json::from_str(arguments.as_str()).map(LocalOperation::BranchCreate)
        }
        LocalToolKind::BranchSwitch => {
            serde_json::from_str(arguments.as_str()).map(LocalOperation::BranchSwitch)
        }
        LocalToolKind::Commit => {
            serde_json::from_str(arguments.as_str()).map(LocalOperation::Commit)
        }
        LocalToolKind::Diff => serde_json::from_str(arguments.as_str()).map(LocalOperation::Diff),
        LocalToolKind::Log => serde_json::from_str(arguments.as_str()).map(LocalOperation::Log),
        LocalToolKind::Stage => serde_json::from_str(arguments.as_str()).map(LocalOperation::Stage),
        LocalToolKind::Status => serde_json::from_str::<GitStatusArguments>(arguments.as_str())
            .map(|_| LocalOperation::Status),
    }
    .map_err(|_| InvalidGitArguments)?;
    validate_operation(&operation)?;
    Ok(operation)
}

pub(super) fn validate_operation(operation: &LocalOperation) -> Result<(), InvalidGitArguments> {
    match operation {
        LocalOperation::BranchCreate(arguments) => {
            validate_branch(&arguments.name)?;
            validate_revision(&arguments.start)
        }
        LocalOperation::BranchSwitch(arguments) => validate_branch(&arguments.name),
        LocalOperation::Commit(arguments) => (arguments.message.len() <= MAX_COMMIT_MESSAGE_BYTES
            && !arguments.message.contains('\0'))
        .then_some(())
        .ok_or(InvalidGitArguments),
        LocalOperation::Diff(GitDiffArguments::Worktree) | LocalOperation::Status => Ok(()),
        LocalOperation::Diff(GitDiffArguments::Revisions { base, head }) => {
            validate_revision(base)?;
            validate_revision(head)
        }
        LocalOperation::Log(arguments) => {
            validate_revision(&arguments.revision)?;
            (arguments.max_entries > 0 && arguments.max_entries <= MAX_LOG_ENTRIES)
                .then_some(())
                .ok_or(InvalidGitArguments)
        }
        LocalOperation::Stage(arguments) => {
            if arguments.paths.is_empty() || arguments.paths.len() > MAX_STAGE_PATHS {
                return Err(InvalidGitArguments);
            }
            let mut unique = HashSet::new();
            if arguments
                .paths
                .iter()
                .all(|path| checked_relative_path(path).is_ok_and(|path| unique.insert(path)))
            {
                Ok(())
            } else {
                Err(InvalidGitArguments)
            }
        }
    }
}

pub(super) fn validate_branch(value: &str) -> Result<(), InvalidGitArguments> {
    if value.is_empty() || value.len() > MAX_BRANCH_BYTES || value.contains('\0') {
        return Err(InvalidGitArguments);
    }
    git2::Reference::is_valid_name(&format!("refs/heads/{value}"))
        .then_some(())
        .ok_or(InvalidGitArguments)
}

pub(super) fn validate_revision(value: &str) -> Result<(), InvalidGitArguments> {
    let exact_object = git2::Oid::from_str(value).is_ok();
    let exact_reference =
        value == "HEAD" || (value.starts_with("refs/") && git2::Reference::is_valid_name(value));
    (value.len() <= MAX_REVISION_BYTES && (exact_object || exact_reference))
        .then_some(())
        .ok_or(InvalidGitArguments)
}

#[derive(Clone, Debug)]
pub(super) struct GitArgumentValidator {
    kind: LocalToolKind,
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for GitArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_operation(self.kind, arguments)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}
