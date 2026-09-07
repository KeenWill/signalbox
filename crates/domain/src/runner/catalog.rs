//! Runner catalog for `docs/spec/runner-protocol.md`.

use super::names::{
    CredentialProfileName, RunnerCapabilityClass, RunnerDomainError, RunnerSelector,
    WorkspaceRepositoryKey, validate_exact,
};
use crate::{
    NormalizedToolArguments, ToolArgumentsKind, ToolEffectClass, ToolName, ToolPermissionDefault,
};
use std::{collections::BTreeMap, collections::BTreeSet};

pub(super) const PERMISSION_OVERRIDE_MAX_ENTRIES: usize = 64;

/// Static nonempty admissible placement for one tool.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ToolAdmissibleLoci {
    /// Allows execution only in the daemon.
    DaemonOnly,
    /// Allows execution only on a runner satisfying the selector.
    RunnerOnly {
        /// The selector a runner must satisfy.
        selector: RunnerSelector,
    },
    /// Allows daemon execution or runner execution satisfying the selector.
    DaemonOrRunner {
        /// The selector a runner must satisfy.
        selector: RunnerSelector,
    },
}

/// Required effect class for runner-admissible tool declarations.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RunnerToolEffectClass {
    /// The tool performs no externally observable effect.
    Pure,
    /// Repetition does not compound the tool's externally visible effect.
    Idempotent,
    /// The runner tool effect may not be safe to repeat.
    SideEffecting,
}

impl ToolAdmissibleLoci {
    /// Reports whether the declaration admits daemon execution.
    pub const fn allows_daemon(&self) -> bool {
        matches!(self, Self::DaemonOnly | Self::DaemonOrRunner { .. })
    }

    /// Returns the required runner selector when runner execution is admissible.
    pub const fn runner_selector(&self) -> Option<&RunnerSelector> {
        match self {
            Self::DaemonOnly => None,
            Self::RunnerOnly { selector } | Self::DaemonOrRunner { selector } => Some(selector),
        }
    }
}

/// Complete daemon-owned policy for an advertisable runner tool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerToolDeclaration {
    pub(super) name: ToolName,
    model: RunnerToolModelDefinition,
    permission: ToolPermissionDefault,
    pub(super) effect: RunnerToolEffectClass,
    pub(super) loci: ToolAdmissibleLoci,
}

impl RunnerToolDeclaration {
    /// Constructs one complete daemon-owned runner tool declaration.
    pub const fn new(
        name: ToolName,
        model: RunnerToolModelDefinition,
        permission: ToolPermissionDefault,
        effect: RunnerToolEffectClass,
        loci: ToolAdmissibleLoci,
    ) -> Self {
        Self {
            name,
            model,
            permission,
            effect,
            loci,
        }
    }

    /// Returns the declared name.
    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    /// Returns the model-facing tool definition.
    pub const fn model(&self) -> &RunnerToolModelDefinition {
        &self.model
    }

    /// Returns the default tool permission.
    pub const fn permission(&self) -> ToolPermissionDefault {
        self.permission
    }

    /// Returns the runner tool effect class.
    pub const fn effect(&self) -> RunnerToolEffectClass {
        self.effect
    }

    /// Returns the admissible execution loci.
    pub const fn loci(&self) -> &ToolAdmissibleLoci {
        &self.loci
    }
}

/// Checked model-facing definition required for every runner-advertisable tool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerToolModelDefinition {
    description: String,
    input_schema: NormalizedToolArguments,
}

impl RunnerToolModelDefinition {
    /// Validates a tool description and normalized JSON object input schema.
    pub fn try_new(description: String, input_schema: String) -> Result<Self, RunnerDomainError> {
        let description = validate_exact(description)?;
        let input_schema = NormalizedToolArguments::try_from_provider_text(input_schema)
            .map_err(|_| RunnerDomainError::InvalidToolInputSchema)?;
        if input_schema.kind() != ToolArgumentsKind::Json || !input_schema.as_str().starts_with('{')
        {
            return Err(RunnerDomainError::InvalidToolInputSchema);
        }
        Ok(Self {
            description,
            input_schema,
        })
    }

    /// Returns the model-facing tool description.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Returns the normalized JSON object input schema.
    pub const fn input_schema(&self) -> &NormalizedToolArguments {
        &self.input_schema
    }
}

/// Approval posture for an exact tool/profile pair.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CredentialToolApproval {
    /// The profile permits automatic approval for the tool.
    Automatic,
    /// The session policy must decide approval for the tool.
    SessionPolicy,
}

/// Daemon-owned approval policy for one profile name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialProfilePolicy {
    pub(super) name: CredentialProfileName,
    approvals: BTreeMap<ToolName, CredentialToolApproval>,
}

impl CredentialProfilePolicy {
    /// Constructs a profile policy while rejecting duplicate tool approvals.
    pub fn try_new(
        name: CredentialProfileName,
        approvals: impl IntoIterator<Item = (ToolName, CredentialToolApproval)>,
    ) -> Result<Self, RunnerDomainError> {
        let mut checked = BTreeMap::new();
        for (tool, approval) in approvals {
            if checked.insert(tool.clone(), approval).is_some() {
                return Err(RunnerDomainError::DuplicateTool(tool));
            }
        }
        Ok(Self {
            name,
            approvals: checked,
        })
    }

    /// Returns the declared name.
    pub const fn name(&self) -> &CredentialProfileName {
        &self.name
    }

    /// Returns the explicit approval posture or the session-policy default for the tool.
    pub fn approval_for(&self, tool: &ToolName) -> CredentialToolApproval {
        self.approvals
            .get(tool)
            .copied()
            .unwrap_or(CredentialToolApproval::SessionPolicy)
    }

    /// Iterates the explicit tool approval overrides.
    pub fn approvals(&self) -> impl Iterator<Item = (&ToolName, CredentialToolApproval)> {
        self.approvals
            .iter()
            .map(|(tool, approval)| (tool, *approval))
    }
}

/// Closed sandbox profiles advertised by runners and selected by placements.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RunnerSandboxProfile {
    /// Supervises execution without restricting the invoking user's filesystem or network.
    Ambient,
    /// Restricts execution to one placement-owned writable root.
    WorkspaceRestricted,
}

/// Session-owned permission override for one exact runner tool.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RunnerToolPermissionOverride {
    /// The exact tool may run without per-attempt confirmation.
    Auto,
    /// The exact tool requires advance user confirmation: an exact user
    /// command, or a one-shot user override of a delegate denial.
    Confirm,
}

/// Checked bounded per-tool permission override inventory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerToolPermissionOverrides(BTreeMap<ToolName, RunnerToolPermissionOverride>);

impl RunnerToolPermissionOverrides {
    /// Constructs at most 64 exact overrides while rejecting duplicate tool names.
    pub fn try_new(
        overrides: impl IntoIterator<Item = (ToolName, RunnerToolPermissionOverride)>,
    ) -> Result<Self, RunnerDomainError> {
        let mut checked = BTreeMap::new();
        for (tool, permission) in overrides {
            if checked.insert(tool.clone(), permission).is_some() {
                return Err(RunnerDomainError::DuplicateTool(tool));
            }
            if checked.len() > PERMISSION_OVERRIDE_MAX_ENTRIES {
                return Err(RunnerDomainError::TooManyPermissionOverrides);
            }
        }
        Ok(Self(checked))
    }

    /// Returns the explicit override for one tool, when present.
    pub fn get(&self, tool: &ToolName) -> Option<RunnerToolPermissionOverride> {
        self.0.get(tool).copied()
    }

    /// Iterates the exact sorted override inventory.
    pub fn iter(&self) -> impl Iterator<Item = (&ToolName, RunnerToolPermissionOverride)> {
        self.0.iter().map(|(tool, permission)| (tool, *permission))
    }
}

/// One advertised repository and its configured credential requirement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerRepositoryEntry {
    pub(super) key: WorkspaceRepositoryKey,
    pub(super) credential_profile: Option<CredentialProfileName>,
}

impl RunnerRepositoryEntry {
    /// Pairs one exact repository key with its optional required profile.
    pub const fn new(
        key: WorkspaceRepositoryKey,
        credential_profile: Option<CredentialProfileName>,
    ) -> Self {
        Self {
            key,
            credential_profile,
        }
    }

    /// Returns the advertised repository key.
    pub const fn key(&self) -> &WorkspaceRepositoryKey {
        &self.key
    }

    /// Returns the configured credential requirement; absence means anonymous HTTPS.
    pub const fn credential_profile(&self) -> Option<&CredentialProfileName> {
        self.credential_profile.as_ref()
    }
}

/// Closed workspace capabilities advertised by runners.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum WorkspaceCapability {
    /// The runner can provision one repository worktree per session.
    WorktreePerSession,
}

/// One complete daemon-authoritative catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerCatalog {
    pub(super) classes: BTreeSet<RunnerCapabilityClass>,
    pub(super) tools: BTreeMap<ToolName, RunnerToolDeclaration>,
    pub(super) profiles: BTreeMap<CredentialProfileName, CredentialProfilePolicy>,
    pub(super) workspaces: BTreeSet<WorkspaceCapability>,
    pub(super) sandboxes: BTreeSet<RunnerSandboxProfile>,
}

impl RunnerCatalog {
    /// Validates and constructs the complete daemon-authoritative runner catalog.
    pub fn try_new(
        classes: impl IntoIterator<Item = RunnerCapabilityClass>,
        tools: impl IntoIterator<Item = RunnerToolDeclaration>,
        profiles: impl IntoIterator<Item = CredentialProfilePolicy>,
        workspaces: impl IntoIterator<Item = WorkspaceCapability>,
        sandboxes: impl IntoIterator<Item = RunnerSandboxProfile>,
    ) -> Result<Self, RunnerDomainError> {
        let mut checked_classes = BTreeSet::new();
        for class in classes {
            if !checked_classes.insert(class.clone()) {
                return Err(RunnerDomainError::DuplicateCapabilityClass(class));
            }
        }
        let mut checked_tools = BTreeMap::new();
        for tool in tools {
            if tool.effect == RunnerToolEffectClass::Idempotent && tool.loci.allows_daemon() {
                return Err(RunnerDomainError::UnsupportedDaemonIdempotency(
                    tool.name.clone(),
                ));
            }
            if let Some(RunnerSelector::CapabilityClass(class)) = tool.loci.runner_selector()
                && !checked_classes.contains(class)
            {
                return Err(RunnerDomainError::CapabilityClassNotAllowed(class.clone()));
            }
            let name = tool.name.clone();
            if checked_tools.insert(name.clone(), tool).is_some() {
                return Err(RunnerDomainError::DuplicateTool(name));
            }
        }
        let mut checked_profiles = BTreeMap::new();
        for profile in profiles {
            let name = profile.name.clone();
            if checked_profiles.insert(name.clone(), profile).is_some() {
                return Err(RunnerDomainError::DuplicateProfile(name));
            }
        }
        for profile in checked_profiles.values() {
            if let Some(tool) = profile
                .approvals
                .keys()
                .find(|tool| !checked_tools.contains_key(*tool))
            {
                return Err(RunnerDomainError::UndeclaredProfileTool(tool.clone()));
            }
        }
        let mut checked_workspaces = BTreeSet::new();
        for workspace in workspaces {
            if !checked_workspaces.insert(workspace) {
                return Err(RunnerDomainError::DuplicateWorkspaceCapability(workspace));
            }
        }
        let mut checked_sandboxes = BTreeSet::new();
        for sandbox in sandboxes {
            if !checked_sandboxes.insert(sandbox) {
                return Err(RunnerDomainError::DuplicateSandboxProfile(sandbox));
            }
        }
        Ok(Self {
            classes: checked_classes,
            tools: checked_tools,
            profiles: checked_profiles,
            workspaces: checked_workspaces,
            sandboxes: checked_sandboxes,
        })
    }
}

/// Availability-only runner advertisement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerAdvertisement {
    pub(super) default_working_directory: Option<crate::RunnerWorkingDirectory>,
    pub(super) classes: BTreeSet<RunnerCapabilityClass>,
    pub(super) tools: BTreeSet<ToolName>,
    pub(super) profiles: BTreeSet<CredentialProfileName>,
    pub(super) workspaces: BTreeSet<WorkspaceCapability>,
    pub(super) sandboxes: BTreeSet<RunnerSandboxProfile>,
    pub(super) repositories: BTreeMap<WorkspaceRepositoryKey, RunnerRepositoryEntry>,
}

impl RunnerAdvertisement {
    /// Maximum repository entries in one runner advertisement.
    pub const MAX_REPOSITORIES: usize = 64;

    /// Collects one availability-only runner advertisement.
    pub fn new(
        classes: impl IntoIterator<Item = RunnerCapabilityClass>,
        tools: impl IntoIterator<Item = ToolName>,
        profiles: impl IntoIterator<Item = CredentialProfileName>,
        workspaces: impl IntoIterator<Item = WorkspaceCapability>,
        sandboxes: impl IntoIterator<Item = RunnerSandboxProfile>,
        repositories: impl IntoIterator<Item = RunnerRepositoryEntry>,
    ) -> Self {
        let repositories = repositories
            .into_iter()
            .map(|entry| (entry.key.clone(), entry))
            .collect();
        Self {
            default_working_directory: None,
            classes: classes.into_iter().collect(),
            tools: tools.into_iter().collect(),
            profiles: profiles.into_iter().collect(),
            workspaces: workspaces.into_iter().collect(),
            sandboxes: sandboxes.into_iter().collect(),
            repositories,
        }
    }

    /// Retains the runner-reported absolute default directory for this advertisement.
    pub fn with_default_working_directory(
        mut self,
        directory: Option<crate::RunnerWorkingDirectory>,
    ) -> Self {
        self.default_working_directory = directory;
        self
    }

    /// Returns the reported default directory, if this runner supplied one.
    pub fn default_working_directory(&self) -> Option<&crate::RunnerWorkingDirectory> {
        self.default_working_directory.as_ref()
    }

    /// Iterates the advertised capability classes in canonical order.
    pub fn classes(&self) -> impl Iterator<Item = &RunnerCapabilityClass> {
        self.classes.iter()
    }

    /// Iterates the advertised tool names in canonical order.
    pub fn tools(&self) -> impl Iterator<Item = &ToolName> {
        self.tools.iter()
    }

    /// Iterates the advertised credential-profile names in canonical order.
    pub fn profiles(&self) -> impl Iterator<Item = &CredentialProfileName> {
        self.profiles.iter()
    }

    /// Iterates the advertised workspace capabilities in canonical order.
    pub fn workspaces(&self) -> impl Iterator<Item = WorkspaceCapability> + '_ {
        self.workspaces.iter().copied()
    }

    /// Iterates the advertised sandbox profiles in canonical order.
    pub fn sandboxes(&self) -> impl Iterator<Item = RunnerSandboxProfile> + '_ {
        self.sandboxes.iter().copied()
    }

    /// Iterates repository entries in canonical key order.
    pub fn repositories(&self) -> impl Iterator<Item = &RunnerRepositoryEntry> {
        self.repositories.values()
    }
}

pub(super) const fn tool_effect_class(effect: RunnerToolEffectClass) -> ToolEffectClass {
    match effect {
        RunnerToolEffectClass::Pure => ToolEffectClass::EffectFree,
        RunnerToolEffectClass::Idempotent | RunnerToolEffectClass::SideEffecting => {
            ToolEffectClass::ExternalEffect
        }
    }
}
