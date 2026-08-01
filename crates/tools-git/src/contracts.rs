use signalbox_application::ToolDefinition;
use signalbox_domain::{ToolEffectClass, ToolPermissionDefault};
use signalbox_tool_contract::{
    ToolContract, ToolContractCompileError, compile_contract_definition,
};

use crate::arguments::{
    GitBranchCreateArguments, GitBranchSwitchArguments, GitCommitArguments, GitDiffArguments,
    GitLogArguments, GitStageArguments, GitStatusArguments,
};
use crate::names::{
    GIT_BRANCH_CREATE_NAME, GIT_BRANCH_SWITCH_NAME, GIT_CREATE_COMMIT_NAME, GIT_DIFF_NAME,
    GIT_LOG_NAME, GIT_STAGE_NAME, GIT_STATUS_NAME,
};

pub(super) struct StatusContract;

impl ToolContract for StatusContract {
    type Arguments = GitStatusArguments;
    const NAME: &'static str = GIT_STATUS_NAME;
    const DESCRIPTION: &'static str =
        "Returns bounded status for the injected repository worktree.";
}

pub(super) struct DiffContract;

impl ToolContract for DiffContract {
    type Arguments = GitDiffArguments;
    const NAME: &'static str = GIT_DIFF_NAME;
    const DESCRIPTION: &'static str =
        "Returns a bounded patch for the injected worktree or between two revisions.";
}

pub(super) struct LogContract;

impl ToolContract for LogContract {
    type Arguments = GitLogArguments;
    const NAME: &'static str = GIT_LOG_NAME;
    const DESCRIPTION: &'static str =
        "Returns bounded commit history from one revision in the injected repository.";
}

pub(super) struct StageContract;

impl ToolContract for StageContract {
    type Arguments = GitStageArguments;
    const NAME: &'static str = GIT_STAGE_NAME;
    const DESCRIPTION: &'static str =
        "Stages exact symlink-safe paths inside the injected repository root.";
}

pub(super) struct CommitContract;

impl ToolContract for CommitContract {
    type Arguments = GitCommitArguments;
    const NAME: &'static str = GIT_CREATE_COMMIT_NAME;
    const DESCRIPTION: &'static str = "Commits the current index with the model-supplied message preserved verbatim and an injected identity.";
}

pub(super) struct BranchCreateContract;

impl ToolContract for BranchCreateContract {
    type Arguments = GitBranchCreateArguments;
    const NAME: &'static str = GIT_BRANCH_CREATE_NAME;
    const DESCRIPTION: &'static str = "Creates one non-forced local branch at an exact revision.";
}

pub(super) struct BranchSwitchContract;

impl ToolContract for BranchSwitchContract {
    type Arguments = GitBranchSwitchArguments;
    const NAME: &'static str = GIT_BRANCH_SWITCH_NAME;
    const DESCRIPTION: &'static str =
        "Safely checks out one existing local branch in the injected worktree.";
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LocalToolKind {
    BranchCreate,
    BranchSwitch,
    Commit,
    Diff,
    Log,
    Stage,
    Status,
}

impl LocalToolKind {
    pub(super) const ALL: [Self; 7] = [
        Self::BranchCreate,
        Self::BranchSwitch,
        Self::Commit,
        Self::Diff,
        Self::Log,
        Self::Stage,
        Self::Status,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::BranchCreate => GIT_BRANCH_CREATE_NAME,
            Self::BranchSwitch => GIT_BRANCH_SWITCH_NAME,
            Self::Commit => GIT_CREATE_COMMIT_NAME,
            Self::Diff => GIT_DIFF_NAME,
            Self::Log => GIT_LOG_NAME,
            Self::Stage => GIT_STAGE_NAME,
            Self::Status => GIT_STATUS_NAME,
        }
    }

    pub(super) const fn effect(self) -> ToolEffectClass {
        match self {
            Self::Diff | Self::Log | Self::Status => ToolEffectClass::EffectFree,
            Self::BranchCreate | Self::BranchSwitch | Self::Commit | Self::Stage => {
                ToolEffectClass::ExternalEffect
            }
        }
    }

    pub(super) fn definition(self) -> Result<ToolDefinition, ToolContractCompileError> {
        match self {
            Self::BranchCreate => compile_contract_definition::<BranchCreateContract>(
                ToolPermissionDefault::Auto,
                self.effect(),
            ),
            Self::BranchSwitch => compile_contract_definition::<BranchSwitchContract>(
                ToolPermissionDefault::Auto,
                self.effect(),
            ),
            Self::Commit => compile_contract_definition::<CommitContract>(
                ToolPermissionDefault::Auto,
                self.effect(),
            ),
            Self::Diff => compile_contract_definition::<DiffContract>(
                ToolPermissionDefault::Auto,
                self.effect(),
            ),
            Self::Log => compile_contract_definition::<LogContract>(
                ToolPermissionDefault::Auto,
                self.effect(),
            ),
            Self::Stage => compile_contract_definition::<StageContract>(
                ToolPermissionDefault::Auto,
                self.effect(),
            ),
            Self::Status => compile_contract_definition::<StatusContract>(
                ToolPermissionDefault::Auto,
                self.effect(),
            ),
        }
    }
}

pub(super) fn kind_for_name(name: &str) -> Option<LocalToolKind> {
    LocalToolKind::ALL
        .into_iter()
        .find(|kind| kind.name() == name)
}
