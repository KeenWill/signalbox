//! Creation-payload runner placement columns for `docs/spec/identity-and-commands.md`.

use serde_json::{Map, Value};
use signalbox_domain::{
    CredentialProfileName, RunnerCapabilityClass, RunnerId, RunnerSandboxProfile, RunnerSelector,
    RunnerToolPermissionOverride, RunnerToolPermissionOverrides, RunnerWorkingDirectory,
    SessionRunnerPlacementRequest, ToolName, WorkingDirectorySelection, WorkspaceRepositoryKey,
    WorkspaceRequirement,
};
use sqlx::types::Uuid;

#[derive(Clone, Debug, Default, sqlx::FromRow)]
pub(crate) struct RunnerPlacementColumns {
    pub(crate) runner_selector_kind: Option<String>,
    pub(crate) runner_selector_id: Option<Uuid>,
    pub(crate) runner_selector_class: Option<String>,
    pub(crate) runner_directory_kind: Option<String>,
    pub(crate) runner_directory: Option<String>,
    pub(crate) runner_credential_profile: Option<String>,
    pub(crate) runner_workspace_kind: Option<String>,
    pub(crate) runner_repository: Option<String>,
    pub(crate) runner_sandbox: Option<String>,
    pub(crate) runner_permission_overrides: Option<Value>,
}

impl RunnerPlacementColumns {
    pub(crate) fn encode(placement: Option<&SessionRunnerPlacementRequest>) -> Self {
        let Some(placement) = placement else {
            return Self::default();
        };
        let (selector_kind, selector_id, selector_class) = match &placement.selector {
            RunnerSelector::Identity(id) => ("identity", Some(id.into_uuid()), None),
            RunnerSelector::CapabilityClass(class) => {
                ("capability_class", None, Some(class.as_str().to_owned()))
            }
        };
        let (directory_kind, directory) = match &placement.working_directory {
            WorkingDirectorySelection::RunnerDefault => ("runner_default", None),
            WorkingDirectorySelection::Exact(directory) => {
                ("exact", Some(directory.as_str().to_owned()))
            }
        };
        let (workspace_kind, repository) = match &placement.workspace {
            WorkspaceRequirement::None => ("none", None),
            WorkspaceRequirement::RepositoryWorktree { repository } => {
                ("repository_worktree", Some(repository.as_str().to_owned()))
            }
        };
        let permissions: Map<String, Value> = placement
            .permission_overrides
            .iter()
            .map(|(tool, permission)| {
                let spelling = match permission {
                    RunnerToolPermissionOverride::Auto => "auto",
                    RunnerToolPermissionOverride::Confirm => "confirm",
                };
                (tool.as_str().to_owned(), Value::String(spelling.to_owned()))
            })
            .collect();
        Self {
            runner_selector_kind: Some(selector_kind.to_owned()),
            runner_selector_id: selector_id,
            runner_selector_class: selector_class,
            runner_directory_kind: Some(directory_kind.to_owned()),
            runner_directory: directory,
            runner_credential_profile: placement
                .credential_profile
                .as_ref()
                .map(|profile| profile.as_str().to_owned()),
            runner_workspace_kind: Some(workspace_kind.to_owned()),
            runner_repository: repository,
            runner_sandbox: Some(
                match placement.sandbox {
                    RunnerSandboxProfile::Ambient => "ambient",
                    RunnerSandboxProfile::WorkspaceRestricted => "workspace_restricted",
                }
                .to_owned(),
            ),
            runner_permission_overrides: Some(Value::Object(permissions)),
        }
    }

    pub(crate) fn decode(
        self,
        version: i16,
        introduced: i16,
    ) -> Result<Option<SessionRunnerPlacementRequest>, &'static str> {
        if self.runner_selector_kind.is_none() {
            if self.runner_selector_id.is_some()
                || self.runner_selector_class.is_some()
                || self.runner_directory_kind.is_some()
                || self.runner_directory.is_some()
                || self.runner_credential_profile.is_some()
                || self.runner_workspace_kind.is_some()
                || self.runner_repository.is_some()
                || self.runner_sandbox.is_some()
                || self.runner_permission_overrides.is_some()
            {
                return Err("absent runner placement columns");
            }
            return Ok(None);
        }
        if version < introduced {
            return Err("runner placement before introduction");
        }
        let selector = match (
            self.runner_selector_kind.as_deref(),
            self.runner_selector_id,
            self.runner_selector_class,
        ) {
            (Some("identity"), Some(id), None) => RunnerSelector::Identity(RunnerId::from_uuid(id)),
            (Some("capability_class"), None, Some(class)) => RunnerSelector::CapabilityClass(
                RunnerCapabilityClass::try_new(class).map_err(|_| "runner selector class")?,
            ),
            _ => return Err("runner selector shape"),
        };
        let working_directory = match (self.runner_directory_kind.as_deref(), self.runner_directory)
        {
            (Some("runner_default"), None) => WorkingDirectorySelection::RunnerDefault,
            (Some("exact"), Some(directory)) => WorkingDirectorySelection::Exact(
                RunnerWorkingDirectory::try_new(directory).map_err(|_| "runner directory")?,
            ),
            _ => return Err("runner directory shape"),
        };
        let workspace = match (
            self.runner_workspace_kind.as_deref(),
            self.runner_repository,
        ) {
            (Some("none"), None) => WorkspaceRequirement::None,
            (Some("repository_worktree"), Some(repository)) => {
                WorkspaceRequirement::RepositoryWorktree {
                    repository: WorkspaceRepositoryKey::try_new(repository)
                        .map_err(|_| "runner repository")?,
                }
            }
            _ => return Err("runner workspace shape"),
        };
        let sandbox = match self.runner_sandbox.as_deref() {
            Some("ambient") => RunnerSandboxProfile::Ambient,
            Some("workspace_restricted") => RunnerSandboxProfile::WorkspaceRestricted,
            _ => return Err("runner sandbox"),
        };
        let Some(Value::Object(permissions)) = self.runner_permission_overrides else {
            return Err("runner permission inventory");
        };
        let permission_overrides = RunnerToolPermissionOverrides::try_new(
            permissions
                .into_iter()
                .map(|(tool, permission)| {
                    let tool = ToolName::try_new(tool).map_err(|_| "runner permission tool")?;
                    let permission = match permission.as_str() {
                        Some("auto") => RunnerToolPermissionOverride::Auto,
                        Some("confirm") => RunnerToolPermissionOverride::Confirm,
                        _ => return Err("runner permission value"),
                    };
                    Ok((tool, permission))
                })
                .collect::<Result<Vec<_>, &'static str>>()?,
        )
        .map_err(|_| "runner permission inventory")?;
        let credential_profile = self
            .runner_credential_profile
            .map(CredentialProfileName::try_new)
            .transpose()
            .map_err(|_| "runner credential profile")?;
        Ok(Some(SessionRunnerPlacementRequest {
            selector,
            working_directory,
            credential_profile,
            workspace,
            sandbox,
            permission_overrides,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placement() -> SessionRunnerPlacementRequest {
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(
                RunnerCapabilityClass::try_new("build".to_owned()).unwrap(),
            ),
            working_directory: WorkingDirectorySelection::Exact(
                RunnerWorkingDirectory::try_new("/workspace/project".to_owned()).unwrap(),
            ),
            credential_profile: Some(CredentialProfileName::try_new("source".to_owned()).unwrap()),
            workspace: WorkspaceRequirement::RepositoryWorktree {
                repository: WorkspaceRepositoryKey::try_new("project".to_owned()).unwrap(),
            },
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            permission_overrides: RunnerToolPermissionOverrides::try_new([
                (
                    ToolName::try_new("read_file".to_owned()).unwrap(),
                    RunnerToolPermissionOverride::Auto,
                ),
                (
                    ToolName::try_new("write_file".to_owned()).unwrap(),
                    RunnerToolPermissionOverride::Confirm,
                ),
            ])
            .unwrap(),
        }
    }

    #[test]
    fn runner_placement_is_retained_from_each_creation_introduction_onward() {
        let expected = placement();
        // These are the permanent field-introduction versions, not the current writer versions.
        for (kind, introduced) in [("create_session", 8), ("imported_creation", 6)] {
            let stored = RunnerPlacementColumns::encode(Some(&expected));
            assert_eq!(
                stored.clone().decode(introduced, introduced),
                Ok(Some(expected.clone())),
                "{kind}"
            );
            assert_eq!(
                stored.clone().decode(introduced + 1, introduced),
                Ok(Some(expected.clone())),
                "{kind}"
            );
            assert_eq!(
                stored.decode(introduced - 1, introduced),
                Err("runner placement before introduction"),
                "{kind}"
            );
            assert_eq!(
                RunnerPlacementColumns::default().decode(introduced - 1, introduced),
                Ok(None),
                "{kind}"
            );
        }
    }

    #[test]
    fn absent_runner_placement_rejects_stray_semantic_columns() {
        let stored = RunnerPlacementColumns {
            runner_credential_profile: Some("source".to_owned()),
            ..RunnerPlacementColumns::default()
        };
        assert_eq!(stored.decode(8, 8), Err("absent runner placement columns"));
    }

    #[test]
    fn runner_placement_rejects_unknown_permission_values() {
        let mut stored = RunnerPlacementColumns::encode(Some(&placement()));
        stored.runner_permission_overrides = Some(serde_json::json!({"read_file": "unknown"}));
        assert_eq!(stored.decode(8, 8), Err("runner permission value"));
    }
}
