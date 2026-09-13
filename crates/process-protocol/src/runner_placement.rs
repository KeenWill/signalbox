//! Caller-supplied runner placement for session creation.

use serde::{Deserialize, Serialize};
use signalbox_domain as domain;

use crate::{
    RunnerCredentialProfileName, RunnerProjectionSelector, RunnerRepositoryKey,
    RunnerSandboxProfile, RunnerWorkingDirectory,
};

/// Independent runner placement axes, retained unpinned at creation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerPlacementRequest {
    /// Runner identity or admitted capability class.
    pub selector: RunnerProjectionSelector,
    /// Runner default or an exact runner-local directory.
    pub working_directory: RunnerDirectorySelection,
    /// An advertised profile name, or no credential grant.
    #[serde(deserialize_with = "crate::deserialize_required_nullable")]
    pub credential_profile: Option<RunnerCredentialProfileName>,
    /// Independent runner workspace requirement.
    pub workspace: RunnerWorkspaceRequirement,
    /// Required advertised execution profile.
    pub sandbox: RunnerSandboxProfile,
    /// Exact per-tool session permission overrides.
    pub permission_overrides: Vec<RunnerToolPermissionOverride>,
}

/// Working directory requested from the selected runner.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerDirectorySelection {
    /// Use the selected runner's reported default.
    RunnerDefault,
    /// Use one exact runner-local directory.
    Exact {
        /// Exact runner-local directory.
        directory: RunnerWorkingDirectory,
    },
}

/// Workspace provisioning requested from the selected runner.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerWorkspaceRequirement {
    /// No runner-owned workspace.
    None,
    /// One session worktree from an advertised repository.
    RepositoryWorktree {
        /// Advertised repository key.
        repository: RunnerRepositoryKey,
    },
}

/// Requested permission for one exact runner tool.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerToolPermissionOverride {
    /// The exact runner tool name.
    pub tool_name: String,
    /// Requested approval posture for that tool.
    pub permission: RunnerToolPermission,
}

/// Session-owned permission posture for a runner tool.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerToolPermission {
    /// Permit policy-based approval.
    Auto,
    /// Require advance user confirmation.
    Confirm,
}

impl RunnerPlacementRequest {
    /// Admits all axes and the duplicate-free bounded permission inventory.
    pub fn try_into_domain(
        self,
    ) -> Result<domain::SessionRunnerPlacementRequest, domain::RunnerDomainError> {
        let selector = match self.selector {
            RunnerProjectionSelector::Runner { runner_id } => {
                domain::RunnerSelector::Identity(domain::RunnerId::from_uuid(runner_id.into_uuid()))
            }
            RunnerProjectionSelector::CapabilityClass { name } => {
                domain::RunnerSelector::CapabilityClass(domain::RunnerCapabilityClass::try_new(
                    name.as_str().to_owned(),
                )?)
            }
        };
        let working_directory = match self.working_directory {
            RunnerDirectorySelection::RunnerDefault => {
                domain::WorkingDirectorySelection::RunnerDefault
            }
            RunnerDirectorySelection::Exact { directory } => {
                domain::WorkingDirectorySelection::Exact(domain::RunnerWorkingDirectory::try_new(
                    directory.as_str().to_owned(),
                )?)
            }
        };
        let workspace = match self.workspace {
            RunnerWorkspaceRequirement::None => domain::WorkspaceRequirement::None,
            RunnerWorkspaceRequirement::RepositoryWorktree { repository } => {
                domain::WorkspaceRequirement::RepositoryWorktree {
                    repository: domain::WorkspaceRepositoryKey::try_new(
                        repository.as_str().to_owned(),
                    )?,
                }
            }
        };
        let permission_overrides = domain::RunnerToolPermissionOverrides::try_new(
            self.permission_overrides
                .into_iter()
                .map(|entry| {
                    let name = domain::ToolName::try_new(entry.tool_name)
                        .map_err(|_| domain::RunnerDomainError::InvalidName)?;
                    let permission = match entry.permission {
                        RunnerToolPermission::Auto => domain::RunnerToolPermissionOverride::Auto,
                        RunnerToolPermission::Confirm => {
                            domain::RunnerToolPermissionOverride::Confirm
                        }
                    };
                    Ok((name, permission))
                })
                .collect::<Result<Vec<_>, domain::RunnerDomainError>>()?,
        )?;
        Ok(domain::SessionRunnerPlacementRequest {
            selector,
            working_directory,
            credential_profile: self
                .credential_profile
                .map(|profile| domain::CredentialProfileName::try_new(profile.as_str().to_owned()))
                .transpose()?,
            workspace,
            sandbox: match self.sandbox {
                RunnerSandboxProfile::Ambient => domain::RunnerSandboxProfile::Ambient,
                RunnerSandboxProfile::WorkspaceRestricted => {
                    domain::RunnerSandboxProfile::WorkspaceRestricted
                }
            },
            permission_overrides,
        })
    }
}
