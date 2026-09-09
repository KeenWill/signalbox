use super::{
    error::HubModelConfigurationError,
    toml_scalars::{reject_unknown_fields, required_string, required_uuid},
};
use signalbox_domain::{DirectModelSelection, InstructionPath, ToolApprovalPosture, ToolName};
use signalbox_tools_exec::{SandboxConfiguration, SandboxNetwork};
use signalbox_tools_git::GitIdentity;
use signalbox_tools_github::{GITHUB_CREDENTIAL_REFERENCE, GitHubEgressPolicy};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    path::{Path, PathBuf},
};
use toml_edit::{Item, Table};

/// Validated deployment dependencies injected into daemon tool families.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonToolConfiguration {
    workspace_root: PathBuf,
    git_identity: GitIdentity,
    exec_supervisor_executable: PathBuf,
    cargo_registry_cache: Option<PathBuf>,
    sandbox: SandboxConfiguration,
}

/// Explicit non-workspace instruction roots registered by deployment configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceInstructionConfiguration {
    pub(super) roots: Box<[InstructionPath]>,
}

impl WorkspaceInstructionConfiguration {
    /// Returns explicit roots in deterministic configuration order.
    pub fn roots(&self) -> &[InstructionPath] {
        &self.roots
    }
}

impl DaemonToolConfiguration {
    /// Absolute root pinned into both workspace tool families.
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Explicit author and committer identity for daemon-local Git commits.
    pub const fn git_identity(&self) -> &GitIdentity {
        &self.git_identity
    }

    /// Absolute existing path of the separately packaged exec supervisor.
    pub fn exec_supervisor_executable(&self) -> &Path {
        &self.exec_supervisor_executable
    }

    /// Optional host Cargo registry pinned read-only into sandboxed execution.
    pub fn cargo_registry_cache(&self) -> Option<&Path> {
        self.cargo_registry_cache.as_deref()
    }

    /// Explicit runtime inputs shared by sandboxed workspace tools.
    pub fn sandbox(&self) -> &SandboxConfiguration {
        &self.sandbox
    }

    /// Fixed public-GitHub-only egress policy selected by the tool registry.
    pub const fn github_egress_policy(&self) -> GitHubEgressPolicy {
        GitHubEgressPolicy::github_api_only()
    }

    /// Non-secret profile shared by both GitHub-backed tool adapters.
    pub const fn github_credential_profile(&self) -> &'static str {
        GITHUB_CREDENTIAL_REFERENCE
    }
}

/// Maximum exact deployment compaction-prompt bytes.
pub const MAX_COMPACTION_PROMPT_UTF8_BYTES: usize = 1_048_576;

pub(super) fn parse_workspace_instruction_configuration(
    item: Option<&Item>,
) -> Result<WorkspaceInstructionConfiguration, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(WorkspaceInstructionConfiguration {
            roots: Box::new([]),
        });
    };
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidWorkspaceInstructionConfiguration)?;
    reject_unknown_fields(table, &["version", "registered_roots"])
        .map_err(|_| HubModelConfigurationError::InvalidWorkspaceInstructionConfiguration)?;
    if table.get("version").and_then(Item::as_integer) != Some(1) {
        return Err(HubModelConfigurationError::InvalidWorkspaceInstructionConfiguration);
    }
    let values = table
        .get("registered_roots")
        .and_then(Item::as_array)
        .ok_or(HubModelConfigurationError::InvalidWorkspaceInstructionConfiguration)?;
    if values.len() > 64 {
        return Err(HubModelConfigurationError::InvalidWorkspaceInstructionConfiguration);
    }
    let mut roots = Vec::with_capacity(values.len());
    let mut unique = BTreeSet::new();
    for value in values {
        let value = value
            .as_str()
            .ok_or(HubModelConfigurationError::InvalidWorkspaceInstructionConfiguration)?;
        let root = InstructionPath::try_new(value.to_owned())
            .map_err(|_| HubModelConfigurationError::InvalidWorkspaceInstructionConfiguration)?;
        if !unique.insert(root.clone()) {
            return Err(HubModelConfigurationError::InvalidWorkspaceInstructionConfiguration);
        }
        roots.push(root);
    }
    Ok(WorkspaceInstructionConfiguration {
        roots: roots.into_boxed_slice(),
    })
}

pub(super) fn parse_tool_approval_postures(
    item: Option<&Item>,
) -> Result<BTreeMap<ToolName, ToolApprovalPosture>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(BTreeMap::new());
    };
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidToolApprovalPostures)?;
    let mut postures = BTreeMap::new();
    for (name, value) in table {
        let name = ToolName::try_new(name.to_owned())
            .map_err(|_| HubModelConfigurationError::InvalidToolApprovalPostures)?;
        let posture = match value.as_str() {
            Some("auto") => ToolApprovalPosture::Auto,
            Some("delegated") => ToolApprovalPosture::Delegated,
            Some("human") => ToolApprovalPosture::Human,
            _ => return Err(HubModelConfigurationError::InvalidToolApprovalPostures),
        };
        postures.insert(name, posture);
    }
    Ok(postures)
}

pub(super) fn parse_approval_judge(
    item: Option<&Item>,
) -> Result<Option<DirectModelSelection>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(None);
    };
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidApprovalJudge)?;
    reject_unknown_fields(table, &["selection_id"])
        .map_err(|_| HubModelConfigurationError::InvalidApprovalJudge)?;
    let selection = required_uuid(table, "selection_id")
        .map_err(|_| HubModelConfigurationError::InvalidApprovalJudge)?;
    Ok(Some(DirectModelSelection::from_uuid(selection)))
}

pub(super) fn parse_tool_mappings(
    item: Option<&Item>,
    git_identity: Option<GitIdentity>,
    daemon_tool_settings: Option<DaemonToolSettings>,
) -> Result<Option<DaemonToolConfiguration>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(None);
    };
    let mappings = item
        .as_array_of_tables()
        .ok_or(HubModelConfigurationError::InvalidToolMappings)?;
    if mappings.is_empty() {
        return Err(HubModelConfigurationError::InvalidToolMappings);
    }
    let mut families = HashSet::with_capacity(mappings.len());
    let mut workspace_root = None;
    for mapping in mappings {
        reject_unknown_fields(
            mapping,
            &[
                "family",
                "adapter",
                "credential_profile",
                "egress_policy",
                "workspace_root",
            ],
        )?;
        let family = required_string(mapping, "family")?;
        if !families.insert(family.to_owned()) {
            return Err(HubModelConfigurationError::DuplicateToolFamily);
        }
        match family {
            "code_host" | "github" => validate_github_tool_mapping(mapping)?,
            "workspace" => {
                validate_workspace_tool_mapping(mapping)?;
                workspace_root = Some(PathBuf::from(required_string(mapping, "workspace_root")?));
            }
            "conversations" => validate_conversation_tool_mapping(mapping)?,
            _ => return Err(HubModelConfigurationError::InvalidToolMappings),
        }
    }
    if families
        != HashSet::from([
            String::from("code_host"),
            String::from("github"),
            String::from("workspace"),
            String::from("conversations"),
        ])
    {
        return Err(HubModelConfigurationError::InvalidToolMappings);
    }
    let settings =
        daemon_tool_settings.ok_or(HubModelConfigurationError::MissingDaemonToolSettings)?;
    Ok(Some(DaemonToolConfiguration {
        workspace_root: workspace_root.ok_or(HubModelConfigurationError::InvalidToolMappings)?,
        git_identity: git_identity
            .ok_or(HubModelConfigurationError::MissingGitIdentityConfiguration)?,
        exec_supervisor_executable: settings.exec_supervisor_executable,
        cargo_registry_cache: settings.cargo_registry_cache,
        sandbox: settings.sandbox,
    }))
}

#[derive(Clone, Debug)]
pub(super) struct DaemonToolSettings {
    exec_supervisor_executable: PathBuf,
    cargo_registry_cache: Option<PathBuf>,
    sandbox: SandboxConfiguration,
}

pub(super) fn parse_daemon_tool_settings(
    item: Option<&Item>,
) -> Result<Option<DaemonToolSettings>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(None);
    };
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidDaemonToolSettings)?;
    reject_unknown_fields(
        table,
        &[
            "exec_supervisor_executable",
            "cargo_registry_cache",
            "sandbox_network",
            "sandbox_read_only_binds",
            "sandbox_path_prepend",
            "sandbox_rustup_home",
            "sandbox_rustup_toolchain",
        ],
    )
    .map_err(|_| HubModelConfigurationError::InvalidDaemonToolSettings)?;
    let executable = PathBuf::from(
        required_string(table, "exec_supervisor_executable")
            .map_err(|_| HubModelConfigurationError::InvalidDaemonToolSettings)?,
    );
    if !executable.is_absolute() {
        return Err(HubModelConfigurationError::InvalidDaemonToolSettings);
    }
    let executable = fs::canonicalize(executable)
        .map_err(|_| HubModelConfigurationError::InvalidDaemonToolSettings)?;
    if !executable.is_file() {
        return Err(HubModelConfigurationError::InvalidDaemonToolSettings);
    }
    let cargo_registry_cache = table
        .get("cargo_registry_cache")
        .map(|_| {
            let path = PathBuf::from(
                required_string(table, "cargo_registry_cache")
                    .map_err(|_| HubModelConfigurationError::InvalidDaemonToolSettings)?,
            );
            if !path.is_absolute() {
                return Err(HubModelConfigurationError::InvalidDaemonToolSettings);
            }
            let canonical = fs::canonicalize(path)
                .map_err(|_| HubModelConfigurationError::InvalidDaemonToolSettings)?;
            if !canonical.is_dir() {
                return Err(HubModelConfigurationError::InvalidDaemonToolSettings);
            }
            Ok(canonical)
        })
        .transpose()?;
    let network = match table.get("sandbox_network").map(Item::as_str) {
        None | Some(Some("none")) => SandboxNetwork::None,
        Some(Some("host")) => SandboxNetwork::Host,
        _ => return Err(HubModelConfigurationError::InvalidDaemonToolSettings),
    };
    let read_only_binds = sandbox_paths(table, "sandbox_read_only_binds")?;
    let path_prepend = sandbox_paths(table, "sandbox_path_prepend")?;
    let rustup_home = table
        .get("sandbox_rustup_home")
        .map(|_| {
            let value = required_string(table, "sandbox_rustup_home")
                .map_err(|_| HubModelConfigurationError::InvalidDaemonToolSettings)?;
            let path = PathBuf::from(value);
            if !path.is_absolute() || !path.is_dir() || value.contains('\0') {
                return Err(HubModelConfigurationError::InvalidDaemonToolSettings);
            }
            Ok(path)
        })
        .transpose()?;
    let rustup_toolchain = table
        .get("sandbox_rustup_toolchain")
        .map(|_| {
            let value = required_string(table, "sandbox_rustup_toolchain")
                .map_err(|_| HubModelConfigurationError::InvalidDaemonToolSettings)?;
            if value.is_empty() || value.contains('\0') {
                return Err(HubModelConfigurationError::InvalidDaemonToolSettings);
            }
            Ok(value.to_owned())
        })
        .transpose()?;
    Ok(Some(DaemonToolSettings {
        exec_supervisor_executable: executable,
        cargo_registry_cache,
        sandbox: SandboxConfiguration {
            network,
            read_only_binds,
            path_prepend,
            rustup_home,
            rustup_toolchain,
        },
    }))
}

fn sandbox_paths(table: &Table, key: &str) -> Result<Vec<PathBuf>, HubModelConfigurationError> {
    let Some(item) = table.get(key) else {
        return Ok(Vec::new());
    };
    let values = item
        .as_array()
        .ok_or(HubModelConfigurationError::InvalidDaemonToolSettings)?;
    values
        .iter()
        .map(|value| {
            let value = value
                .as_str()
                .ok_or(HubModelConfigurationError::InvalidDaemonToolSettings)?;
            let path = PathBuf::from(value);
            if !path.is_absolute()
                || value.contains('\0')
                || !path.exists()
                || (key == "sandbox_path_prepend" && (!path.is_dir() || value.contains(':')))
            {
                return Err(HubModelConfigurationError::InvalidDaemonToolSettings);
            }
            Ok(path)
        })
        .collect()
}

pub(super) fn parse_git_identity(
    item: Option<&Item>,
) -> Result<Option<GitIdentity>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(None);
    };
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidGitIdentityConfiguration)?;
    reject_unknown_fields(table, &["author_name", "author_email"])
        .map_err(|_| HubModelConfigurationError::InvalidGitIdentityConfiguration)?;
    let author_name = required_string(table, "author_name")
        .map_err(|_| HubModelConfigurationError::InvalidGitIdentityConfiguration)?;
    let author_email = required_string(table, "author_email")
        .map_err(|_| HubModelConfigurationError::InvalidGitIdentityConfiguration)?;
    GitIdentity::try_new(author_name, author_email)
        .map(Some)
        .map_err(|_| HubModelConfigurationError::InvalidGitIdentityConfiguration)
}

fn validate_github_tool_mapping(mapping: &Table) -> Result<(), HubModelConfigurationError> {
    if required_string(mapping, "adapter")? != "github"
        || required_string(mapping, "credential_profile")? != GITHUB_CREDENTIAL_REFERENCE
        || required_string(mapping, "egress_policy")? != "github_api_only"
        || mapping.get("workspace_root").is_some()
    {
        return Err(HubModelConfigurationError::InvalidToolMappings);
    }
    Ok(())
}

fn validate_workspace_tool_mapping(mapping: &Table) -> Result<(), HubModelConfigurationError> {
    let root_value = required_string(mapping, "workspace_root")?;
    let root = Path::new(root_value);
    if required_string(mapping, "adapter")? != "local"
        || !root.is_absolute()
        || InstructionPath::try_new(root_value.to_owned()).is_err()
        || mapping.get("credential_profile").is_some()
        || mapping.get("egress_policy").is_some()
    {
        return Err(HubModelConfigurationError::InvalidToolMappings);
    }
    Ok(())
}

fn validate_conversation_tool_mapping(mapping: &Table) -> Result<(), HubModelConfigurationError> {
    if required_string(mapping, "adapter")? != "application"
        || mapping.get("credential_profile").is_some()
        || mapping.get("egress_policy").is_some()
        || mapping.get("workspace_root").is_some()
    {
        return Err(HubModelConfigurationError::InvalidToolMappings);
    }
    Ok(())
}
