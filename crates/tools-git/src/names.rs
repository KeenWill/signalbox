/// Repository status tool name.
pub const GIT_STATUS_NAME: &str = "git_status";

/// Repository diff tool name.
pub const GIT_DIFF_NAME: &str = "git_diff";

/// Repository history tool name.
pub const GIT_LOG_NAME: &str = "git_log";

/// Index staging tool name.
pub const GIT_STAGE_NAME: &str = "git_stage";

/// Commit creation tool name.
pub const GIT_CREATE_COMMIT_NAME: &str = "git_create_commit";

/// Local branch creation tool name.
pub const GIT_BRANCH_CREATE_NAME: &str = "git_branch_create";

/// Local branch switch tool name.
pub const GIT_BRANCH_SWITCH_NAME: &str = "git_branch_switch";

/// Fixed local-family catalog order.
pub const LOCAL_GIT_TOOL_NAMES: [&str; 7] = [
    GIT_BRANCH_CREATE_NAME,
    GIT_BRANCH_SWITCH_NAME,
    GIT_CREATE_COMMIT_NAME,
    GIT_DIFF_NAME,
    GIT_LOG_NAME,
    GIT_STAGE_NAME,
    GIT_STATUS_NAME,
];
