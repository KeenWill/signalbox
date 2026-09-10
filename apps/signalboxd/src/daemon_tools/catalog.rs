use super::DaemonToolsConstructionError;
use crate::{blob_tools::BLOB_TOOL_NAMES, goal_mode::GOAL_DECLARE_NAME};
use signalbox_application::{
    CompiledToolCatalog, ToolCatalog, ToolCatalogValidationFailure, ToolDefinition,
};
use signalbox_domain::{NormalizedToolArguments, ToolApprovalPosture, ToolName};
use signalbox_tools_basic::{CURRENT_TIME_NAME, ECHO_NAME, SESSION_STATUS_UPDATE_NAME};
use signalbox_tools_code_host::CODE_HOST_TOOL_NAMES;
use signalbox_tools_conversations::CONVERSATION_TOOL_NAMES;
use signalbox_tools_exec::{CARGO_DIAGNOSTICS_NAME, SANDBOXED_EXEC_NAME, UNSANDBOXED_EXEC_NAME};
use signalbox_tools_git::LOCAL_GIT_TOOL_NAMES;
use signalbox_tools_github::GITHUB_TOOL_NAMES;
use signalbox_tools_plan::PLAN_TOOL_NAMES;
use signalbox_tools_sessions::SESSION_DELEGATION_TOOL_NAMES;
use signalbox_tools_web::{WEB_FETCH_NAME, WEB_SEARCH_NAME};
use signalbox_tools_workspace::{WORKSPACE_MUTATION_TOOL_NAMES, WORKSPACE_READ_TOOL_NAMES};
use std::{collections::BTreeMap, error::Error, fmt};

#[derive(Clone, Debug)]
pub(super) struct DaemonToolCatalogEntry {
    definition: ToolDefinition,
    catalog: CompiledToolCatalog,
}

/// Stable merged view of independently compiled daemon tool modules.
#[derive(Clone, Debug)]
pub struct DaemonToolCatalog {
    pub(super) entries: BTreeMap<ToolName, DaemonToolCatalogEntry>,
}

/// Statically selected daemon tool families available before runtime assembly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DaemonToolComposition {
    /// Process-local and always-compiled tool families only.
    Base,
    /// Base tools plus families enabled by complete deployment mappings.
    WithMappedFamilies,
}

impl DaemonToolCatalog {
    pub(super) fn try_new(
        catalogs: impl IntoIterator<Item = CompiledToolCatalog>,
    ) -> Result<Self, DuplicateDaemonTool> {
        let mut entries = BTreeMap::new();
        for catalog in catalogs {
            for definition in catalog.definitions() {
                let name = definition.name().clone();
                if entries
                    .insert(
                        name.clone(),
                        DaemonToolCatalogEntry {
                            definition,
                            catalog: catalog.clone(),
                        },
                    )
                    .is_some()
                {
                    return Err(DuplicateDaemonTool);
                }
            }
        }
        Ok(Self { entries })
    }

    /// Registers configured push for mapped workspaces when a watched repository enables it.
    pub fn with_repository_push(
        self,
        configuration: Option<&crate::RepositoryWatchConfiguration>,
        composition: DaemonToolComposition,
    ) -> Result<Self, DaemonToolsConstructionError> {
        if composition == DaemonToolComposition::WithMappedFamilies
            && configuration.is_some_and(|watch| {
                watch
                    .repositories()
                    .iter()
                    .any(|repository| repository.push_credential_file().is_some())
            })
        {
            self.with_compiled_catalog(
                signalbox_tools_git::git_push_catalog()
                    .map_err(|_| DaemonToolsConstructionError::LocalGit)?,
            )
        } else {
            Ok(self)
        }
    }

    /// Validates deployment postures against the statically selected
    /// composition before database-backed tool dependencies are constructed.
    pub fn validate_approval_postures_for_composition(
        postures: impl IntoIterator<Item = (ToolName, ToolApprovalPosture)>,
        composition: DaemonToolComposition,
    ) -> Result<(), ConfiguredApprovalPostureError> {
        for (name, _posture) in postures {
            if !configured_composition_contains(&name, composition) {
                return Err(ConfiguredApprovalPostureError::UnknownTool { name });
            }
        }
        Ok(())
    }

    /// Applies explicit deployment postures that the current runtime can enforce.
    pub fn with_approval_postures(
        mut self,
        postures: impl IntoIterator<Item = (ToolName, ToolApprovalPosture)>,
    ) -> Result<Self, ConfiguredApprovalPostureError> {
        for (name, posture) in postures {
            let Some(entry) = self.entries.get_mut(&name) else {
                return Err(ConfiguredApprovalPostureError::UnknownTool { name });
            };
            let posture = match (name.as_str(), posture) {
                ("web_fetch" | "web_search", ToolApprovalPosture::Auto) => {
                    ToolApprovalPosture::Delegated
                }
                _ => posture,
            };
            entry.definition = entry.definition.clone().with_approval_posture(posture);
        }
        Ok(self)
    }

    /// Extends the immutable daemon registry with one compiled family.
    pub fn with_compiled_catalog(
        mut self,
        catalog: CompiledToolCatalog,
    ) -> Result<Self, DaemonToolsConstructionError> {
        for definition in catalog.definitions() {
            let name = definition.name().clone();
            if self
                .entries
                .insert(
                    name.clone(),
                    DaemonToolCatalogEntry {
                        definition,
                        catalog: catalog.clone(),
                    },
                )
                .is_some()
            {
                return Err(DaemonToolsConstructionError::Duplicate);
            }
        }
        Ok(self)
    }
}

fn configured_composition_contains(name: &ToolName, composition: DaemonToolComposition) -> bool {
    let name = name.as_str();
    let mapped_family_contains = match composition {
        DaemonToolComposition::Base => false,
        DaemonToolComposition::WithMappedFamilies => {
            GITHUB_TOOL_NAMES.contains(&name)
                || WORKSPACE_READ_TOOL_NAMES.contains(&name)
                || WORKSPACE_MUTATION_TOOL_NAMES.contains(&name)
                || LOCAL_GIT_TOOL_NAMES.contains(&name)
                || name == signalbox_tools_git::GIT_PUSH_CONFIGURED_NAME
                || matches!(
                    name,
                    SANDBOXED_EXEC_NAME | UNSANDBOXED_EXEC_NAME | CARGO_DIAGNOSTICS_NAME
                )
                || CONVERSATION_TOOL_NAMES.contains(&name)
        }
    };
    name == CURRENT_TIME_NAME
        || name == ECHO_NAME
        || name == WEB_FETCH_NAME
        || name == WEB_SEARCH_NAME
        || name == SESSION_STATUS_UPDATE_NAME
        || name == GOAL_DECLARE_NAME
        || CODE_HOST_TOOL_NAMES.contains(&name)
        || PLAN_TOOL_NAMES.contains(&name)
        || SESSION_DELEGATION_TOOL_NAMES.contains(&name)
        || BLOB_TOOL_NAMES.contains(&name)
        || matches!(
            name,
            signalbox_tools_file_media::FILE_INSPECT_NAME
                | signalbox_tools_file_media::FILE_READ_NAME
        )
        || mapped_family_contains
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DuplicateDaemonTool;

/// A configured approval posture cannot be enforced by this daemon runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfiguredApprovalPostureError {
    /// The configured name is absent from the composed catalog.
    UnknownTool { name: ToolName },
}

impl ConfiguredApprovalPostureError {
    /// Borrows the configured tool name without exposing it to startup telemetry.
    pub const fn name(&self) -> &ToolName {
        match self {
            Self::UnknownTool { name } => name,
        }
    }
}

impl fmt::Display for ConfiguredApprovalPostureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnknownTool { .. } => "configured approval posture names an unknown tool",
        })
    }
}

impl Error for ConfiguredApprovalPostureError {}

impl ToolCatalog for DaemonToolCatalog {
    fn definitions(&self) -> Box<[ToolDefinition]> {
        self.entries
            .values()
            .map(|entry| entry.definition.clone())
            .collect()
    }

    fn definition(&self, name: &ToolName) -> Option<ToolDefinition> {
        self.entries.get(name).map(|entry| entry.definition.clone())
    }

    fn validate_arguments(
        &self,
        name: &ToolName,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolCatalogValidationFailure> {
        self.entries
            .get(name)
            .ok_or(ToolCatalogValidationFailure::UnknownTool)?
            .catalog
            .validate_arguments(name, arguments)
    }

    fn preauthorization(
        &self,
        name: &ToolName,
        arguments: &NormalizedToolArguments,
    ) -> Result<signalbox_application::ToolPreauthorization, ToolCatalogValidationFailure> {
        self.entries
            .get(name)
            .ok_or(ToolCatalogValidationFailure::UnknownTool)?
            .catalog
            .preauthorization(name, arguments)
    }
}
