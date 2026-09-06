//! Runner enrollment, catalog, lease, placement, and credential grants.
//!
//! The normative specification is `docs/spec/runner-protocol.md`.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use crate::{
    ApprovedToolRequest, AuthorizedToolAttempt, EndedToolAttempt, NormalizedToolArguments,
    RunnerAuthenticationId, RunnerEnrollmentId, RunnerId, RunnerLeaseId, SessionId,
    ToolArgumentsKind, ToolAttemptDispatchCorrelation, ToolAttemptId, ToolBatch,
    ToolBatchExecutionFailure, ToolDecisionSource, ToolEffectClass, ToolName,
    ToolPermissionDefault, WorkspaceManifestId,
};

const NAME_MAX_BYTES: usize = 64;
const EXACT_VALUE_MAX_BYTES: usize = 4_096;
const PERMISSION_OVERRIDE_MAX_ENTRIES: usize = 64;
const WORKSPACE_BRANCH_MAX_BYTES: usize = 255;

/// Why runner domain input or stored facts fail closed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunnerDomainError {
    /// The supplied text is empty.
    Empty,
    /// The supplied text contains a null byte.
    ContainsNull,
    /// The supplied text exceeds its byte limit.
    TooLong,
    /// The supplied portable name has invalid syntax.
    InvalidName,
    /// The supplied digest or revision is not canonical lowercase hexadecimal text.
    InvalidHex,
    /// The supplied Git branch name is not a canonical branch ref name.
    InvalidBranchName,
    /// The supplied runner-root-relative path is not canonical.
    InvalidRelativePath,
    /// The tool input schema is not a normalized JSON object.
    InvalidToolInputSchema,
    /// A capability class appears more than once.
    DuplicateCapabilityClass(RunnerCapabilityClass),
    /// A tool name appears more than once.
    DuplicateTool(ToolName),
    /// A credential profile appears more than once.
    DuplicateProfile(CredentialProfileName),
    /// A workspace capability appears more than once.
    DuplicateWorkspaceCapability(WorkspaceCapability),
    /// A sandbox profile appears more than once.
    DuplicateSandboxProfile(RunnerSandboxProfile),
    /// The placement contains too many per-tool permission overrides.
    TooManyPermissionOverrides,
    /// The advertisement contains too many repository entries.
    TooManyAdvertisedRepositories,
    /// A credential profile names a tool absent from the catalog.
    UndeclaredProfileTool(ToolName),
    /// An idempotent tool is incorrectly admissible on the daemon.
    UnsupportedDaemonIdempotency(ToolName),
    /// The runner enrollment has been revoked.
    EnrollmentRevoked,
    /// The enrollment or catalog does not allow the capability class.
    CapabilityClassNotAllowed(RunnerCapabilityClass),
    /// The runner advertised a tool absent from the catalog.
    ToolUndeclared(ToolName),
    /// The runner does not satisfy the tool placement policy.
    ToolLocusNotAllowed(ToolName),
    /// The runner advertised a credential profile absent from the catalog.
    CredentialProfileUndeclared(CredentialProfileName),
    /// The runner advertised a workspace capability absent from the catalog.
    WorkspaceCapabilityNotAllowed(WorkspaceCapability),
    /// The runner advertised a sandbox profile absent from the catalog.
    SandboxProfileNotAllowed(RunnerSandboxProfile),
    /// A repository entry requires a profile absent from the same advertisement.
    RepositoryProfileUnavailable(CredentialProfileName),
    /// The requested transition is invalid from the current state.
    InvalidState,
    /// Supplied facts do not correlate with the authoritative aggregate.
    CorrelationMismatch,
    /// A positive generation has no representable successor.
    GenerationExhausted,
    /// A retry reused an existing physical attempt identity.
    AttemptIdentityReuse,
    /// The selected runner does not satisfy the placement request.
    SelectorMismatch,
    /// The requested credential profile is unavailable on the selected runner.
    CredentialProfileUnavailable,
    /// The supplied working directory differs from the pinned directory.
    WorkingDirectoryMismatch,
    /// The selected runner lacks the required workspace capability.
    WorkspaceCapabilityUnavailable,
    /// The selected runner lacks the required sandbox profile.
    SandboxProfileUnavailable,
    /// The selected runner lacks the requested repository entry.
    RepositoryUnavailable,
    /// The provisioned workspace does not match the placement request.
    WorkspaceMismatch,
    /// A required tool is unavailable on the selected runner.
    ToolUnavailable,
    /// The credential grant has been revoked.
    GrantRevoked,
    /// The runner registration is no longer the current revision.
    RegistrationChanged,
    /// Another registration preparation already holds the enrollment fence.
    RegistrationInProgress,
    /// Independently stored facts disagree during reconstitution.
    CorruptStoredFacts,
}

fn validate_name(value: String) -> Result<String, RunnerDomainError> {
    if value.is_empty() {
        return Err(RunnerDomainError::Empty);
    }
    if value.contains('\0') {
        return Err(RunnerDomainError::ContainsNull);
    }
    if value.len() > NAME_MAX_BYTES {
        return Err(RunnerDomainError::TooLong);
    }
    if !value
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(RunnerDomainError::InvalidName);
    }
    Ok(value)
}

fn validate_exact(value: String) -> Result<String, RunnerDomainError> {
    if value.is_empty() {
        return Err(RunnerDomainError::Empty);
    }
    if value.contains('\0') {
        return Err(RunnerDomainError::ContainsNull);
    }
    if value.len() > EXACT_VALUE_MAX_BYTES {
        return Err(RunnerDomainError::TooLong);
    }
    Ok(value)
}

fn validate_lower_hex(value: String, lengths: &[usize]) -> Result<String, RunnerDomainError> {
    if !lengths.contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(RunnerDomainError::InvalidHex);
    }
    Ok(value)
}

/// A daemon-defined class used to target an unpinned runner.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunnerCapabilityClass(String);

impl RunnerCapabilityClass {
    /// Validates and constructs a portable runner capability class.
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError> {
        validate_name(value).map(Self)
    }

    /// Returns the validated capability class text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A runner-local credential profile represented to the daemon by name only.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CredentialProfileName(String);

impl CredentialProfileName {
    /// Validates and constructs a portable credential profile name.
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError> {
        validate_name(value).map(Self)
    }

    /// Returns the validated credential profile name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Exact runner-interpreted working-directory text.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunnerWorkingDirectory(String);

impl RunnerWorkingDirectory {
    /// Maximum UTF-8 bytes admitted by an exact runner working directory.
    pub const MAX_BYTES: usize = EXACT_VALUE_MAX_BYTES;

    /// Validates and constructs exact runner working-directory text.
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError> {
        validate_exact(value).map(Self)
    }

    /// Returns the exact runner working-directory text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Exact repository key used for worktree provisioning.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceRepositoryKey(String);

impl WorkspaceRepositoryKey {
    /// Validates and constructs a portable workspace repository key.
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError> {
        validate_name(value).map(Self)
    }

    /// Returns the validated repository key.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Canonical lowercase SHA-256 identity of one configuration-validated clone URL.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CanonicalCloneUrlDigest(String);

impl CanonicalCloneUrlDigest {
    /// Validates and constructs a canonical clone-URL digest.
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError> {
        validate_lower_hex(value, &[64]).map(Self)
    }

    /// Returns the canonical lowercase hexadecimal digest.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Canonical full Git object identity used to recover a workspace.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceRevision(String);

impl WorkspaceRevision {
    /// Validates a full SHA-1 or SHA-256 Git object identity.
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError> {
        validate_lower_hex(value, &[40, 64]).map(Self)
    }

    /// Returns the canonical lowercase full object identity.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Validated Git branch name, without the `refs/heads/` prefix.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceBranchName(String);

impl WorkspaceBranchName {
    /// Validates the branch as the complete `refs/heads/<name>` ref form.
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError> {
        let invalid_component = value.split('/').any(|component| {
            component.is_empty() || component.starts_with('.') || component.ends_with(".lock")
        });
        if value.is_empty()
            || value.len() > WORKSPACE_BRANCH_MAX_BYTES
            || value == "@"
            || value.starts_with('-')
            || value.ends_with('.')
            || value.contains("..")
            || value.contains("@{")
            || value.bytes().any(|byte| {
                byte <= 0x20
                    || byte == 0x7f
                    || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
            })
            || invalid_component
        {
            return Err(RunnerDomainError::InvalidBranchName);
        }
        Ok(Self(value))
    }

    /// Returns the validated branch name without `refs/heads/`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Runner-root-relative path recorded in a workspace manifest.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceRelativePath(String);

impl WorkspaceRelativePath {
    /// Validates a bounded nonempty relative path without traversal components.
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError> {
        let exact = validate_exact(value)?;
        if exact.starts_with('/')
            || exact
                .split('/')
                .any(|component| component.is_empty() || matches!(component, "." | ".."))
        {
            return Err(RunnerDomainError::InvalidRelativePath);
        }
        Ok(Self(exact))
    }

    /// Returns the exact runner-root-relative path.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Exclusive Git recovery facts retained by a repository workspace.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum WorkspaceRecovery {
    /// Recovery checks out one exact detached commit.
    Commit {
        /// The exact commit to recover.
        revision: WorkspaceRevision,
    },
    /// Recovery checks out one validated branch at its exact revision.
    Branch {
        /// The validated branch name without `refs/heads/`.
        name: WorkspaceBranchName,
        /// The exact revision the branch must name.
        revision: WorkspaceRevision,
    },
}

/// Class-or-identity runner targeting.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RunnerSelector {
    /// Selects one exact runner identity.
    Identity(RunnerId),
    /// Selects any runner advertising the required capability class.
    CapabilityClass(RunnerCapabilityClass),
}

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
    name: ToolName,
    model: RunnerToolModelDefinition,
    permission: ToolPermissionDefault,
    effect: RunnerToolEffectClass,
    loci: ToolAdmissibleLoci,
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
    name: CredentialProfileName,
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
    key: WorkspaceRepositoryKey,
    credential_profile: Option<CredentialProfileName>,
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
    classes: BTreeSet<RunnerCapabilityClass>,
    tools: BTreeMap<ToolName, RunnerToolDeclaration>,
    profiles: BTreeMap<CredentialProfileName, CredentialProfilePolicy>,
    workspaces: BTreeSet<WorkspaceCapability>,
    sandboxes: BTreeSet<RunnerSandboxProfile>,
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
    classes: BTreeSet<RunnerCapabilityClass>,
    tools: BTreeSet<ToolName>,
    profiles: BTreeSet<CredentialProfileName>,
    workspaces: BTreeSet<WorkspaceCapability>,
    sandboxes: BTreeSet<RunnerSandboxProfile>,
    repositories: BTreeMap<WorkspaceRepositoryKey, RunnerRepositoryEntry>,
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
            classes: classes.into_iter().collect(),
            tools: tools.into_iter().collect(),
            profiles: profiles.into_iter().collect(),
            workspaces: workspaces.into_iter().collect(),
            sandboxes: sandboxes.into_iter().collect(),
            repositories,
        }
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

/// Active or terminally revoked logical enrollment.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RunnerEnrollmentState {
    /// The enrollment may register runner availability.
    Active,
    /// The enrollment is terminally unable to register.
    Revoked,
}

/// Logical enrollment; identity never derives from machine properties.
#[derive(Debug)]
pub struct RunnerEnrollment {
    enrollment: RunnerEnrollmentId,
    runner: RunnerId,
    authentication: RunnerAuthenticationId,
    allowed_classes: BTreeSet<RunnerCapabilityClass>,
    state: RunnerEnrollmentState,
    registration_revision: Arc<AtomicU64>,
    registration_active: Arc<AtomicBool>,
    registration_preparation: Arc<AtomicBool>,
}

impl PartialEq for RunnerEnrollment {
    fn eq(&self, other: &Self) -> bool {
        self.enrollment == other.enrollment
            && self.runner == other.runner
            && self.authentication == other.authentication
            && self.allowed_classes == other.allowed_classes
            && self.state == other.state
            && self.registration_revision.load(Ordering::Acquire)
                == other.registration_revision.load(Ordering::Acquire)
    }
}

impl Eq for RunnerEnrollment {}

impl RunnerEnrollment {
    /// Creates an active logical enrollment with no issued registration revision.
    pub fn new(
        enrollment: RunnerEnrollmentId,
        runner: RunnerId,
        authentication: RunnerAuthenticationId,
        allowed_classes: impl IntoIterator<Item = RunnerCapabilityClass>,
    ) -> Self {
        Self {
            enrollment,
            runner,
            authentication,
            allowed_classes: allowed_classes.into_iter().collect(),
            state: RunnerEnrollmentState::Active,
            registration_revision: Arc::new(AtomicU64::new(0)),
            registration_active: Arc::new(AtomicBool::new(true)),
            registration_preparation: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns the logical enrollment identity.
    pub const fn enrollment(&self) -> RunnerEnrollmentId {
        self.enrollment
    }

    /// Returns the runner identity.
    pub const fn runner(&self) -> RunnerId {
        self.runner
    }

    /// Returns the runner authentication reference.
    pub const fn authentication(&self) -> RunnerAuthenticationId {
        self.authentication
    }

    /// Returns the current domain state.
    pub const fn state(&self) -> RunnerEnrollmentState {
        self.state
    }

    /// Iterates the capability classes this enrollment permits.
    pub fn allowed_classes(&self) -> impl Iterator<Item = &RunnerCapabilityClass> {
        self.allowed_classes.iter()
    }

    /// The last registration revision this enrollment authority issued, or
    /// `None` while the enrollment is pristine and has issued none.
    pub fn last_issued_registration_revision(&self) -> Option<RunnerGeneration> {
        RunnerGeneration::try_from_u64(self.registration_revision.load(Ordering::Acquire))
    }

    /// Transitions the value to its terminal revoked state.
    pub fn revoke(mut self) -> Result<Self, RunnerDomainError> {
        self.revoke_in_place()?;
        Ok(self)
    }

    /// Revokes the enrollment while preserving its shared registration fences.
    pub fn revoke_in_place(&mut self) -> Result<(), RunnerDomainError> {
        if self.state != RunnerEnrollmentState::Active {
            return Err(RunnerDomainError::InvalidState);
        }
        self.state = RunnerEnrollmentState::Revoked;
        self.registration_active.store(false, Ordering::Release);
        Ok(())
    }

    /// Validates and atomically commits one runner advertisement.
    pub fn register(
        &self,
        advertisement: RunnerAdvertisement,
        catalog: &RunnerCatalog,
    ) -> Result<ValidatedRunnerRegistration, RunnerDomainError> {
        self.prepare_registration(advertisement, catalog)?.commit()
    }

    /// Validates an advertisement and reserves its next registration revision.
    pub fn prepare_registration(
        &self,
        advertisement: RunnerAdvertisement,
        catalog: &RunnerCatalog,
    ) -> Result<PreparedRunnerRegistration, RunnerDomainError> {
        if self.state != RunnerEnrollmentState::Active {
            return Err(RunnerDomainError::EnrollmentRevoked);
        }
        if advertisement.repositories.len() > RunnerAdvertisement::MAX_REPOSITORIES {
            return Err(RunnerDomainError::TooManyAdvertisedRepositories);
        }
        // At most one outstanding preparation exists per enrollment
        // authority, so nothing can advance the shared registration revision
        // between this snapshot and the preparation's commit: an adapter that
        // commits durable rows first can then always advance the fence.
        self.registration_preparation
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| RunnerDomainError::RegistrationInProgress)?;
        let preparation = RegistrationPreparationGuard(Arc::clone(&self.registration_preparation));
        if let Some(class) = advertisement.classes.iter().find(|class| {
            !self.allowed_classes.contains(*class) || !catalog.classes.contains(*class)
        }) {
            return Err(RunnerDomainError::CapabilityClassNotAllowed(class.clone()));
        }
        if let Some(tool) = advertisement
            .tools
            .iter()
            .find(|tool| !catalog.tools.contains_key(*tool))
        {
            return Err(RunnerDomainError::ToolUndeclared(tool.clone()));
        }
        if let Some(tool) = advertisement.tools.iter().find(|tool| {
            let Some(declaration) = catalog.tools.get(*tool) else {
                return true;
            };
            match declaration.loci.runner_selector() {
                Some(RunnerSelector::Identity(runner)) => runner != &self.runner,
                Some(RunnerSelector::CapabilityClass(class)) => {
                    !advertisement.classes.contains(class)
                }
                None => true,
            }
        }) {
            return Err(RunnerDomainError::ToolLocusNotAllowed(tool.clone()));
        }
        if let Some(profile) = advertisement
            .profiles
            .iter()
            .find(|profile| !catalog.profiles.contains_key(*profile))
        {
            return Err(RunnerDomainError::CredentialProfileUndeclared(
                profile.clone(),
            ));
        }
        if let Some(workspace) = advertisement
            .workspaces
            .iter()
            .find(|workspace| !catalog.workspaces.contains(*workspace))
        {
            return Err(RunnerDomainError::WorkspaceCapabilityNotAllowed(*workspace));
        }
        if let Some(sandbox) = advertisement
            .sandboxes
            .iter()
            .find(|sandbox| !catalog.sandboxes.contains(*sandbox))
        {
            return Err(RunnerDomainError::SandboxProfileNotAllowed(*sandbox));
        }
        if let Some(profile) = advertisement.repositories.values().find_map(|entry| {
            entry
                .credential_profile
                .as_ref()
                .filter(|profile| !advertisement.profiles.contains(*profile))
        }) {
            return Err(RunnerDomainError::RepositoryProfileUnavailable(
                profile.clone(),
            ));
        }
        let mut tools = BTreeMap::new();
        for name in advertisement.tools {
            let Some(declaration) = catalog.tools.get(&name) else {
                return Err(RunnerDomainError::ToolUndeclared(name));
            };
            tools.insert(name, declaration.clone());
        }
        let mut profiles = BTreeMap::new();
        for name in advertisement.profiles {
            let Some(policy) = catalog.profiles.get(&name) else {
                return Err(RunnerDomainError::CredentialProfileUndeclared(name));
            };
            profiles.insert(name, policy.clone());
        }
        let prior_revision = self.registration_revision.load(Ordering::Acquire);
        let revision = prior_revision
            .checked_add(1)
            .and_then(RunnerGeneration::try_from_u64)
            .ok_or(RunnerDomainError::GenerationExhausted)?;
        Ok(PreparedRunnerRegistration {
            expected_revision: prior_revision,
            preparation,
            registration: ValidatedRunnerRegistration {
                enrollment: self.enrollment,
                runner: self.runner,
                authentication: self.authentication,
                catalog_tools: catalog.tools.keys().cloned().collect(),
                classes: advertisement.classes,
                tools,
                profiles,
                workspaces: advertisement.workspaces,
                sandboxes: advertisement.sandboxes,
                repositories: advertisement.repositories,
                revision,
                current_revision: Arc::clone(&self.registration_revision),
                enrollment_active: Arc::clone(&self.registration_active),
            },
        })
    }

    fn authorizes(
        &self,
        registration: &ValidatedRunnerRegistration,
    ) -> Result<(), RunnerDomainError> {
        if self.state != RunnerEnrollmentState::Active {
            return Err(RunnerDomainError::EnrollmentRevoked);
        }
        if self.enrollment != registration.enrollment
            || self.runner != registration.runner
            || self.authentication != registration.authentication
            || !Arc::ptr_eq(&self.registration_revision, &registration.current_revision)
            || !Arc::ptr_eq(&self.registration_active, &registration.enrollment_active)
        {
            return Err(RunnerDomainError::CorrelationMismatch);
        }
        if !registration.is_current() {
            return Err(RunnerDomainError::RegistrationChanged);
        }
        Ok(())
    }

    /// Reconstitutes an enrollment after cross-checking independently stored facts.
    pub fn reconstitute(
        input: RunnerEnrollmentReconstitutionInput,
    ) -> Result<Self, RunnerDomainError> {
        if input.enrollment != input.recorded_enrollment
            || input.runner != input.recorded_runner
            || input.authentication != input.recorded_authentication
            || input.allowed_classes != input.recorded_allowed_classes
            || input.registration_revision != input.recorded_registration_revision
            || input.state != input.recorded_state
        {
            return Err(RunnerDomainError::CorruptStoredFacts);
        }
        let registration_revision = input.registration_revision.map_or(0, RunnerGeneration::get);
        Ok(Self {
            enrollment: input.enrollment,
            runner: input.runner,
            authentication: input.authentication,
            allowed_classes: input.allowed_classes,
            state: input.state,
            registration_revision: Arc::new(AtomicU64::new(registration_revision)),
            registration_active: Arc::new(AtomicBool::new(
                input.state == RunnerEnrollmentState::Active,
            )),
            registration_preparation: Arc::new(AtomicBool::new(false)),
        })
    }
}

/// Releases the enrollment-shared exclusive preparation fence when the
/// prepared registration commits or is abandoned without committing.
#[derive(Debug)]
struct RegistrationPreparationGuard(Arc<AtomicBool>);

impl Drop for RegistrationPreparationGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// One validated registration awaiting its authoritative commit point. It
/// holds the enrollment's exclusive preparation fence, so no concurrent
/// registration can advance the shared revision before this one commits or
/// is abandoned.
#[derive(Debug)]
pub struct PreparedRunnerRegistration {
    expected_revision: u64,
    preparation: RegistrationPreparationGuard,
    registration: ValidatedRunnerRegistration,
}

impl PreparedRunnerRegistration {
    /// Returns the validated registration awaiting commit.
    pub const fn registration(&self) -> &ValidatedRunnerRegistration {
        &self.registration
    }

    /// Commits the reserved registration revision and releases its preparation fence.
    pub fn commit(self) -> Result<ValidatedRunnerRegistration, RunnerDomainError> {
        let Self {
            expected_revision,
            preparation,
            registration,
        } = self;
        if !registration.enrollment_active.load(Ordering::Acquire) {
            return Err(RunnerDomainError::EnrollmentRevoked);
        }
        registration
            .current_revision
            .compare_exchange(
                expected_revision,
                registration.revision.get(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| RunnerDomainError::RegistrationChanged)?;
        // Release the preparation fence only after the shared revision has
        // advanced, so a successor preparation always snapshots the committed
        // revision.
        drop(preparation);
        Ok(registration)
    }
}

/// Complete independently stored enrollment facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerEnrollmentReconstitutionInput {
    /// The logical enrollment identity.
    pub enrollment: RunnerEnrollmentId,
    /// The independently recorded enrollment used to cross-check the projection.
    pub recorded_enrollment: RunnerEnrollmentId,
    /// The runner bound to this enrollment.
    pub runner: RunnerId,
    /// The independently recorded runner used to cross-check the projection.
    pub recorded_runner: RunnerId,
    /// The runner authentication reference.
    pub authentication: RunnerAuthenticationId,
    /// The independently recorded authentication used to cross-check the projection.
    pub recorded_authentication: RunnerAuthenticationId,
    /// The capability classes permitted or advertised by the enrollment.
    pub allowed_classes: BTreeSet<RunnerCapabilityClass>,
    /// The independently recorded allowed classes used to cross-check the projection.
    pub recorded_allowed_classes: BTreeSet<RunnerCapabilityClass>,
    /// The last registration revision issued by the enrollment, if any.
    pub registration_revision: Option<RunnerGeneration>,
    /// The independently recorded registration revision used to cross-check the projection.
    pub recorded_registration_revision: Option<RunnerGeneration>,
    /// The stored domain state.
    pub state: RunnerEnrollmentState,
    /// The independently recorded state used to cross-check the projection.
    pub recorded_state: RunnerEnrollmentState,
}

/// Validated availability paired with daemon-owned policy.
#[derive(Clone, Debug)]
pub struct ValidatedRunnerRegistration {
    enrollment: RunnerEnrollmentId,
    runner: RunnerId,
    authentication: RunnerAuthenticationId,
    catalog_tools: BTreeSet<ToolName>,
    classes: BTreeSet<RunnerCapabilityClass>,
    tools: BTreeMap<ToolName, RunnerToolDeclaration>,
    profiles: BTreeMap<CredentialProfileName, CredentialProfilePolicy>,
    workspaces: BTreeSet<WorkspaceCapability>,
    sandboxes: BTreeSet<RunnerSandboxProfile>,
    repositories: BTreeMap<WorkspaceRepositoryKey, RunnerRepositoryEntry>,
    revision: RunnerGeneration,
    current_revision: Arc<AtomicU64>,
    enrollment_active: Arc<AtomicBool>,
}

impl PartialEq for ValidatedRunnerRegistration {
    fn eq(&self, other: &Self) -> bool {
        self.enrollment == other.enrollment
            && self.runner == other.runner
            && self.authentication == other.authentication
            && self.catalog_tools == other.catalog_tools
            && self.classes == other.classes
            && self.tools == other.tools
            && self.profiles == other.profiles
            && self.workspaces == other.workspaces
            && self.sandboxes == other.sandboxes
            && self.repositories == other.repositories
            && self.revision == other.revision
    }
}

impl Eq for ValidatedRunnerRegistration {}

impl ValidatedRunnerRegistration {
    /// Returns the logical enrollment identity.
    pub const fn enrollment(&self) -> RunnerEnrollmentId {
        self.enrollment
    }

    /// Returns the runner identity.
    pub const fn runner(&self) -> RunnerId {
        self.runner
    }

    /// Returns the runner authentication reference.
    pub const fn authentication(&self) -> RunnerAuthenticationId {
        self.authentication
    }

    /// Returns this registration revision.
    pub const fn revision(&self) -> RunnerGeneration {
        self.revision
    }

    fn is_current(&self) -> bool {
        self.enrollment_active.load(Ordering::Acquire)
            && self.current_revision.load(Ordering::Acquire) == self.revision.get()
    }

    /// Reports whether this registration satisfies the runner selector.
    pub fn satisfies(&self, selector: &RunnerSelector) -> bool {
        match selector {
            RunnerSelector::Identity(runner) => self.runner == *runner,
            RunnerSelector::CapabilityClass(class) => self.classes.contains(class),
        }
    }

    /// Returns the complete declaration for a registered tool.
    pub fn tool(&self, tool: &ToolName) -> Option<&RunnerToolDeclaration> {
        self.tools.get(tool)
    }

    /// Returns the declared credential profile policy when present.
    pub fn profile(&self, profile: &CredentialProfileName) -> Option<&CredentialProfilePolicy> {
        self.profiles.get(profile)
    }

    /// Reports whether the runner advertised the workspace capability.
    pub fn supports_workspace(&self, capability: WorkspaceCapability) -> bool {
        self.workspaces.contains(&capability)
    }

    /// Reports whether the runner advertised the sandbox profile.
    pub fn supports_sandbox(&self, profile: RunnerSandboxProfile) -> bool {
        self.sandboxes.contains(&profile)
    }

    /// Returns the exact advertised repository entry, when present.
    pub fn repository(&self, key: &WorkspaceRepositoryKey) -> Option<&RunnerRepositoryEntry> {
        self.repositories.get(key)
    }

    /// Iterates the registered tool names.
    pub fn tool_names(&self) -> impl Iterator<Item = &ToolName> {
        self.tools.keys()
    }

    /// Iterates the registered capability classes.
    pub fn classes(&self) -> impl Iterator<Item = &RunnerCapabilityClass> {
        self.classes.iter()
    }

    /// Iterates the complete tool set.
    pub fn tools(&self) -> impl Iterator<Item = &RunnerToolDeclaration> {
        self.tools.values()
    }

    /// Iterates the registered credential profile policies.
    pub fn profiles(&self) -> impl Iterator<Item = &CredentialProfilePolicy> {
        self.profiles.values()
    }

    /// Iterates the advertised workspace capabilities.
    pub fn workspaces(&self) -> impl Iterator<Item = WorkspaceCapability> + '_ {
        self.workspaces.iter().copied()
    }

    /// Iterates the advertised sandbox profiles.
    pub fn sandboxes(&self) -> impl Iterator<Item = RunnerSandboxProfile> + '_ {
        self.sandboxes.iter().copied()
    }

    /// Iterates the advertised repository entries in key order.
    pub fn repositories(&self) -> impl Iterator<Item = &RunnerRepositoryEntry> {
        self.repositories.values()
    }

    /// Reconstitutes validated availability against the enrollment and current catalog.
    pub fn reconstitute(
        enrollment: &RunnerEnrollment,
        catalog: &RunnerCatalog,
        input: ValidatedRunnerRegistrationReconstitutionInput,
    ) -> Result<Self, RunnerDomainError> {
        if enrollment.enrollment != input.enrollment
            || enrollment.runner != input.runner
            || enrollment.authentication != input.authentication
        {
            return Err(RunnerDomainError::CorruptStoredFacts);
        }
        let revision = input.revision;
        let advertisement = RunnerAdvertisement::new(
            input.classes.clone(),
            input.tools.iter().map(|tool| tool.name.clone()),
            input.profiles.iter().map(|profile| profile.name.clone()),
            input.workspaces.clone(),
            input.sandboxes.clone(),
            input.repositories.clone(),
        );
        let stored_tool_count = input.tools.len();
        let stored_tools: BTreeMap<_, _> = input
            .tools
            .into_iter()
            .map(|tool| (tool.name.clone(), tool))
            .collect();
        let stored_profile_count = input.profiles.len();
        let stored_profiles: BTreeMap<_, _> = input
            .profiles
            .into_iter()
            .map(|profile| (profile.name.clone(), profile))
            .collect();
        let stored_repository_count = input.repositories.len();
        let stored_repositories: BTreeMap<_, _> = input
            .repositories
            .into_iter()
            .map(|entry| (entry.key.clone(), entry))
            .collect();
        let historical_authority = RunnerEnrollment {
            enrollment: enrollment.enrollment,
            runner: enrollment.runner,
            authentication: enrollment.authentication,
            allowed_classes: enrollment.allowed_classes.clone(),
            state: RunnerEnrollmentState::Active,
            registration_revision: Arc::new(AtomicU64::new(0)),
            registration_active: Arc::new(AtomicBool::new(true)),
            registration_preparation: Arc::new(AtomicBool::new(false)),
        };
        let mut registration = historical_authority
            .prepare_registration(advertisement, catalog)
            .map_err(|_| RunnerDomainError::CorruptStoredFacts)?
            .registration;
        if stored_tools.len() != stored_tool_count
            || stored_profiles.len() != stored_profile_count
            || registration.classes != input.classes
            || registration.tools != stored_tools
            || registration.profiles != stored_profiles
            || registration.workspaces != input.workspaces
            || registration.sandboxes != input.sandboxes
            || stored_repositories.len() != stored_repository_count
            || registration.repositories != stored_repositories
        {
            return Err(RunnerDomainError::CorruptStoredFacts);
        }
        registration.revision = revision;
        registration.current_revision = Arc::clone(&enrollment.registration_revision);
        registration.enrollment_active = Arc::clone(&enrollment.registration_active);
        Ok(registration)
    }
}

/// Complete validated-registration facts loaded from canonical storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedRunnerRegistrationReconstitutionInput {
    /// The logical enrollment identity.
    pub enrollment: RunnerEnrollmentId,
    /// The stored registration revision.
    pub revision: RunnerGeneration,
    /// The runner advertised by the stored registration.
    pub runner: RunnerId,
    /// The runner authentication reference.
    pub authentication: RunnerAuthenticationId,
    /// The exact advertised capability classes.
    pub classes: BTreeSet<RunnerCapabilityClass>,
    /// The exact tools advertised by the stored registration.
    pub tools: Vec<RunnerToolDeclaration>,
    /// The exact advertised credential profile policies.
    pub profiles: Vec<CredentialProfilePolicy>,
    /// The exact advertised workspace capabilities.
    pub workspaces: BTreeSet<WorkspaceCapability>,
    /// The exact advertised sandbox profiles.
    pub sandboxes: BTreeSet<RunnerSandboxProfile>,
    /// The exact advertised repository entries.
    pub repositories: Vec<RunnerRepositoryEntry>,
}

/// Positive runner lease, placement, or grant generation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunnerGeneration(NonZeroU64);

impl RunnerGeneration {
    /// Returns the first positive runner generation.
    pub const fn one() -> Self {
        Self(NonZeroU64::MIN)
    }

    /// Converts a nonzero integer into a runner generation.
    pub const fn try_from_u64(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the positive generation as an integer.
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// Returns the next generation when the integer range permits it.
    pub const fn checked_next(self) -> Option<Self> {
        match self.get().checked_add(1) {
            Some(value) => match NonZeroU64::new(value) {
                Some(value) => Some(Self(value)),
                None => None,
            },
            None => None,
        }
    }
}

/// Exact lease claim/result fence.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RunnerLeaseCorrelation {
    /// The logical runner lease identity assigned to the offer or correlation.
    pub lease: RunnerLeaseId,
    /// The runner assigned this lease.
    pub runner: RunnerId,
    /// The exact tool name.
    pub tool: ToolName,
    /// The exact tool-attempt dispatch correlation.
    pub dispatch: ToolAttemptDispatchCorrelation,
    /// The lease fence generation.
    pub generation: RunnerGeneration,
}

/// Complete caller-supplied identities for one initial lease offer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerLeaseOfferRequest {
    /// The logical runner lease identity assigned to the offer or correlation.
    pub lease: RunnerLeaseId,
    /// The exact tool name.
    pub tool: ToolName,
}

#[derive(Debug, Eq, PartialEq)]
struct ClaimedAttemptReplacementEvidence {
    source: RunnerLeaseCorrelation,
    replacement: ToolAttemptDispatchCorrelation,
}

#[derive(Debug, Eq, PartialEq)]
enum RunnerRetryAttemptEvidence {
    Unclaimed {
        dispatch: ToolAttemptDispatchCorrelation,
    },
    Claimed(ClaimedAttemptReplacementEvidence),
}

/// Single-use tool-loop authority bound to its approved tool request.
///
/// Canonical request pairing is owned by [`ToolBatch`], not by callers:
///
/// ```compile_fail
/// use signalbox_domain::{
///     ApprovedToolRequest, AuthorizedToolAttempt, RunnerToolAttemptAuthorization,
/// };
///
/// fn substitute_request(approved: ApprovedToolRequest, authorized: AuthorizedToolAttempt) {
///     let _ = RunnerToolAttemptAuthorization::try_new(approved, authorized);
/// }
/// ```
#[derive(Debug, Eq, PartialEq)]
pub struct RunnerToolAttemptAuthorization {
    approved: ApprovedToolRequest,
    authorized: AuthorizedToolAttempt,
    retry_evidence: Option<RunnerRetryAttemptEvidence>,
}

impl RunnerToolAttemptAuthorization {
    pub(crate) fn try_new(
        approved: ApprovedToolRequest,
        authorized: AuthorizedToolAttempt,
    ) -> Result<Self, RunnerDomainError> {
        let request = approved.request();
        let correlation = authorized.correlation();
        if request.id() != correlation.request()
            || request.session() != correlation.session()
            || request.turn() != correlation.turn()
        {
            return Err(RunnerDomainError::CorrelationMismatch);
        }
        if !authorized.claim_runner_issuance() {
            return Err(RunnerDomainError::InvalidState);
        }
        Ok(Self {
            approved,
            authorized,
            retry_evidence: None,
        })
    }

    /// Returns the approved tool name bound to this authorization.
    pub const fn tool(&self) -> &ToolName {
        self.approved.request().name()
    }

    fn into_parts(
        self,
    ) -> (
        ApprovedToolRequest,
        AuthorizedToolAttempt,
        Option<RunnerRetryAttemptEvidence>,
    ) {
        (self.approved, self.authorized, self.retry_evidence)
    }
}

/// Runner lease stage independent of a streaming connection.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RunnerLeaseState {
    /// The lease was offered but has not been claimed.
    Offered,
    /// The runner claimed the lease and may have executed it.
    Claimed,
    /// The claimed lease completed successfully.
    Completed,
    /// The lease was lost with proof that execution authority was never issued.
    LostUnclaimed,
    /// The offered lease was lost without proof that execution was impossible.
    LostExecutionPossible,
    /// The claimed lease was lost before a completion result arrived.
    LostClaimed,
}

/// Durable authority proving that one offered lease never issued execution capability.
///
/// ```compile_fail
/// use signalbox_domain::{RunnerLeaseCorrelation, RunnerLeaseNoExecutionProof};
///
/// fn fabricate(correlation: RunnerLeaseCorrelation) {
///     let _ = RunnerLeaseNoExecutionProof { correlation };
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerLeaseNoExecutionProof {
    correlation: RunnerLeaseCorrelation,
}

impl RunnerLeaseNoExecutionProof {
    /// Returns the complete lease claim and result fence.
    pub const fn correlation(&self) -> &RunnerLeaseCorrelation {
        &self.correlation
    }
}

/// One fenced runner lease.
#[derive(Debug, Eq, PartialEq)]
pub struct RunnerLease {
    lease: RunnerLeaseId,
    dispatch: ToolAttemptDispatchCorrelation,
    runner: RunnerId,
    tool: ToolName,
    effect: RunnerToolEffectClass,
    credential_authorization: Option<CredentialDispatchAuthorization>,
    generation: RunnerGeneration,
    state: RunnerLeaseState,
}

impl RunnerLease {
    fn offer_validated(input: ValidatedRunnerLeaseOffer) -> Self {
        Self {
            lease: input.lease,
            dispatch: input.dispatch,
            runner: input.runner,
            tool: input.tool,
            effect: input.effect,
            credential_authorization: input.credential_authorization,
            generation: input.generation,
            state: RunnerLeaseState::Offered,
        }
    }

    /// Returns the complete lease claim and result fence.
    pub fn correlation(&self) -> RunnerLeaseCorrelation {
        RunnerLeaseCorrelation {
            lease: self.lease,
            runner: self.runner,
            tool: self.tool.clone(),
            dispatch: self.dispatch,
            generation: self.generation,
        }
    }

    /// Returns the lease lifecycle state.
    pub const fn state(&self) -> RunnerLeaseState {
        self.state
    }

    /// Returns the lease fence generation.
    pub const fn generation(&self) -> RunnerGeneration {
        self.generation
    }

    /// Returns the physical tool-attempt identity.
    pub const fn attempt(&self) -> ToolAttemptId {
        self.dispatch.attempt()
    }

    /// Returns the tool leased for runner execution.
    pub const fn tool(&self) -> &ToolName {
        &self.tool
    }

    /// Returns the runner-bound credential authorization when required.
    pub const fn credential_authorization(&self) -> Option<&CredentialDispatchAuthorization> {
        self.credential_authorization.as_ref()
    }

    /// Returns the owning session identity.
    pub const fn session(&self) -> SessionId {
        self.dispatch.session()
    }

    /// Returns the runner identity.
    pub const fn runner(&self) -> RunnerId {
        self.runner
    }

    /// Returns the runner tool effect class.
    pub const fn effect(&self) -> RunnerToolEffectClass {
        self.effect
    }

    /// Claims the offered lease under the exact supplied correlation fence.
    pub fn claim(mut self, correlation: RunnerLeaseCorrelation) -> Result<Self, RunnerDomainError> {
        if self.state != RunnerLeaseState::Offered {
            return Err(RunnerDomainError::InvalidState);
        }
        if self.correlation() != correlation {
            return Err(RunnerDomainError::CorrelationMismatch);
        }
        self.state = RunnerLeaseState::Claimed;
        Ok(self)
    }

    /// Completes the claimed lease under the exact supplied correlation fence.
    pub fn complete(
        mut self,
        correlation: RunnerLeaseCorrelation,
    ) -> Result<Self, RunnerDomainError> {
        if self.state != RunnerLeaseState::Claimed {
            return Err(RunnerDomainError::InvalidState);
        }
        if self.correlation() != correlation {
            return Err(RunnerDomainError::CorrelationMismatch);
        }
        self.state = RunnerLeaseState::Completed;
        Ok(self)
    }

    /// Classifies loss when runner execution may have occurred.
    pub fn lose(mut self) -> Result<RunnerLeaseLoss, RunnerDomainError> {
        if !matches!(
            self.state,
            RunnerLeaseState::Offered | RunnerLeaseState::Claimed
        ) {
            return Err(RunnerDomainError::InvalidState);
        }
        self.state = match self.state {
            RunnerLeaseState::Offered => RunnerLeaseState::LostExecutionPossible,
            RunnerLeaseState::Claimed => RunnerLeaseState::LostClaimed,
            _ => return Err(RunnerDomainError::InvalidState),
        };
        self.into_loss_consequence(None, RunnerLeaseRetryPreparation::Available)
    }

    /// Classifies loss using proof that execution authority was never issued.
    pub fn lose_unclaimed(
        mut self,
        proof: &RunnerLeaseNoExecutionProof,
    ) -> Result<RunnerLeaseLoss, RunnerDomainError> {
        if self.state != RunnerLeaseState::Offered {
            return Err(RunnerDomainError::InvalidState);
        }
        if proof.correlation != self.correlation() {
            return Err(RunnerDomainError::CorrelationMismatch);
        }
        self.state = RunnerLeaseState::LostUnclaimed;
        self.into_loss_consequence(Some(proof.clone()), RunnerLeaseRetryPreparation::Available)
    }

    fn into_loss_consequence(
        self,
        no_execution: Option<RunnerLeaseNoExecutionProof>,
        retry_preparation: RunnerLeaseRetryPreparation,
    ) -> Result<RunnerLeaseLoss, RunnerDomainError> {
        let claimed = match (self.state, no_execution.is_some()) {
            (RunnerLeaseState::LostUnclaimed, true) => false,
            (RunnerLeaseState::LostExecutionPossible | RunnerLeaseState::LostClaimed, false) => {
                true
            }
            _ => return Err(RunnerDomainError::InvalidState),
        };
        if claimed && self.effect == RunnerToolEffectClass::SideEffecting {
            return Ok(RunnerLeaseLoss {
                kind: RunnerLeaseLossKind::CrashClassificationRequired { lost: self },
            });
        }
        let generation = self
            .generation
            .checked_next()
            .ok_or(RunnerDomainError::GenerationExhausted)?;
        let claimed_attempt = claimed.then_some(self.dispatch.attempt());
        let source = RunnerLeaseRetrySource::from_lease(&self);
        Ok(RunnerLeaseLoss {
            kind: RunnerLeaseLossKind::RetryPermitted {
                lost: self,
                retry: Box::new(RunnerLeaseRetryAuthority {
                    source,
                    generation,
                    claimed_attempt,
                    preparation: RunnerRetryPreparationGuard::new(retry_preparation),
                }),
                no_execution,
            },
        })
    }

    /// Reconstitutes a lease after checking its independent fence facts and registration.
    pub fn reconstitute(
        input: RunnerLeaseReconstitutionInput,
        registration: &ValidatedRunnerRegistration,
    ) -> Result<Self, RunnerDomainError> {
        let lease = Self {
            lease: input.lease,
            dispatch: input.dispatch,
            runner: input.runner,
            tool: input.tool,
            effect: input.effect,
            credential_authorization: input.credential_authorization,
            generation: input.generation,
            state: input.state,
        };
        let credential_matches =
            lease
                .credential_authorization
                .as_ref()
                .is_none_or(|authorization| {
                    authorization.session == lease.dispatch.session()
                        && authorization.runner == lease.runner
                        && authorization.tool == lease.tool
                });
        let declaration_matches = registration.runner == lease.runner
            && registration
                .tool(&lease.tool)
                .is_some_and(|declaration| declaration.effect == lease.effect);
        if lease.correlation() != input.recorded_correlation
            || lease.dispatch.session() != input.recorded_session
            || lease.effect != input.recorded_effect
            || lease.credential_authorization != input.recorded_credential_authorization
            || !credential_matches
            || !declaration_matches
            || lease.state != input.recorded_state
        {
            return Err(RunnerDomainError::CorruptStoredFacts);
        }
        Ok(lease)
    }

    /// Reconstitutes a lost lease and its retry consequence.
    pub fn reconstitute_loss(
        input: RunnerLeaseReconstitutionInput,
        registration: &ValidatedRunnerRegistration,
        no_execution: Option<RunnerLeaseCorrelation>,
    ) -> Result<RunnerLeaseLoss, RunnerDomainError> {
        let retry_preparation = input.retry_preparation;
        Self::reconstitute(input, registration)?
            .into_reconstituted_loss(no_execution, retry_preparation)
    }

    /// Restores the checked loss consequence for an already reconstituted lease.
    pub fn into_reconstituted_loss(
        self,
        no_execution: Option<RunnerLeaseCorrelation>,
        retry_preparation: RunnerLeaseRetryPreparation,
    ) -> Result<RunnerLeaseLoss, RunnerDomainError> {
        let proof_matches = no_execution
            .as_ref()
            .is_some_and(|correlation| *correlation == self.correlation());
        match (self.state, proof_matches, no_execution.is_some()) {
            (RunnerLeaseState::LostUnclaimed, true, true)
            | (
                RunnerLeaseState::LostExecutionPossible | RunnerLeaseState::LostClaimed,
                false,
                false,
            ) => self.into_loss_consequence(
                no_execution.map(|correlation| RunnerLeaseNoExecutionProof { correlation }),
                retry_preparation,
            ),
            _ => Err(RunnerDomainError::InvalidState),
        }
    }
}

struct ValidatedRunnerLeaseOffer {
    lease: RunnerLeaseId,
    dispatch: ToolAttemptDispatchCorrelation,
    runner: RunnerId,
    tool: ToolName,
    effect: RunnerToolEffectClass,
    credential_authorization: Option<CredentialDispatchAuthorization>,
    generation: RunnerGeneration,
}

/// Complete lease projection plus independently stored fence facts.
#[derive(Debug, Eq, PartialEq)]
pub struct RunnerLeaseReconstitutionInput {
    /// The logical runner lease identity assigned to the offer or correlation.
    pub lease: RunnerLeaseId,
    /// The exact tool-attempt dispatch correlation.
    pub dispatch: ToolAttemptDispatchCorrelation,
    /// The runner recorded as the lease owner.
    pub runner: RunnerId,
    /// The exact tool name.
    pub tool: ToolName,
    /// The runner tool effect class checked against the registration.
    pub effect: RunnerToolEffectClass,
    /// The runner-bound credential authorization, when required.
    pub credential_authorization: Option<CredentialDispatchAuthorization>,
    /// The lease fence generation.
    pub generation: RunnerGeneration,
    /// The stored domain state.
    pub state: RunnerLeaseState,
    /// The independently recorded correlation used to cross-check the projection.
    pub recorded_correlation: RunnerLeaseCorrelation,
    /// The independently recorded session used to cross-check the projection.
    pub recorded_session: SessionId,
    /// The independently recorded effect used to cross-check the projection.
    pub recorded_effect: RunnerToolEffectClass,
    /// The independently recorded credential authorization used to cross-check the projection.
    pub recorded_credential_authorization: Option<CredentialDispatchAuthorization>,
    /// The independently recorded state used to cross-check the projection.
    pub recorded_state: RunnerLeaseState,
    /// Whether the single-use retry preparation remains available.
    pub retry_preparation: RunnerLeaseRetryPreparation,
}

/// Whether a lost lease's single-use retry preparation remains available.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RunnerLeaseRetryPreparation {
    /// No retry successor has been prepared from this lost lease.
    Available,
    /// The lost lease has already prepared its one permitted retry successor.
    Prepared,
}

/// Typed consequence of lease loss. Construction is sealed to checked `RunnerLease` transitions.
///
/// ```compile_fail
/// use signalbox_domain::RunnerLeaseLoss;
///
/// fn fabricate() {
///     let _ = RunnerLeaseLoss::CrashClassificationRequired { lost: todo!() };
/// }
/// ```
#[derive(Debug, Eq, PartialEq)]
pub struct RunnerLeaseLoss {
    kind: RunnerLeaseLossKind,
}

#[derive(Debug, Eq, PartialEq)]
enum RunnerLeaseLossKind {
    RetryPermitted {
        lost: RunnerLease,
        retry: Box<RunnerLeaseRetryAuthority>,
        no_execution: Option<RunnerLeaseNoExecutionProof>,
    },
    CrashClassificationRequired {
        lost: RunnerLease,
    },
}

impl RunnerLeaseLoss {
    /// Returns the lease snapshot classified as lost.
    pub const fn lost(&self) -> &RunnerLease {
        match &self.kind {
            RunnerLeaseLossKind::RetryPermitted { lost, .. }
            | RunnerLeaseLossKind::CrashClassificationRequired { lost } => lost,
        }
    }

    /// Returns retry authority when the loss classification permits retry.
    pub const fn retry(&self) -> Option<&RunnerLeaseRetryAuthority> {
        match &self.kind {
            RunnerLeaseLossKind::RetryPermitted { retry, .. } => Some(retry),
            RunnerLeaseLossKind::CrashClassificationRequired { .. } => None,
        }
    }

    /// Returns the attempt requiring crash classification for a side-effecting loss.
    pub const fn crash_attempt(&self) -> Option<ToolAttemptId> {
        match &self.kind {
            RunnerLeaseLossKind::RetryPermitted { .. } => None,
            RunnerLeaseLossKind::CrashClassificationRequired { lost } => {
                Some(lost.dispatch.attempt())
            }
        }
    }

    /// Returns proof that the unclaimed lease never issued execution authority.
    pub const fn no_execution_proof(&self) -> Option<&RunnerLeaseNoExecutionProof> {
        match &self.kind {
            RunnerLeaseLossKind::RetryPermitted { no_execution, .. } => no_execution.as_ref(),
            RunnerLeaseLossKind::CrashClassificationRequired { .. } => None,
        }
    }

    fn into_retry_parts(self) -> Option<(RunnerLease, RunnerLeaseRetryAuthority)> {
        match self.kind {
            RunnerLeaseLossKind::RetryPermitted { lost, retry, .. } => Some((lost, *retry)),
            RunnerLeaseLossKind::CrashClassificationRequired { .. } => None,
        }
    }
}

/// One checked unclaimed-retry batch successor for the never-executed attempt.
#[derive(Debug, Eq, PartialEq)]
pub struct RunnerUnclaimedAttemptReauthorization {
    batch: ToolBatch,
    authorization: RunnerToolAttemptAuthorization,
}

impl RunnerUnclaimedAttemptReauthorization {
    /// Returns the checked successor tool batch.
    pub const fn batch(&self) -> &ToolBatch {
        &self.batch
    }

    /// Consumes the checked result into its correlated parts.
    pub fn into_parts(self) -> (ToolBatch, RunnerToolAttemptAuthorization) {
        (self.batch, self.authorization)
    }
}

/// One checked claimed-retry batch successor with both physical attempts.
#[derive(Debug, Eq, PartialEq)]
pub struct RunnerClaimedAttemptReplacement {
    batch: ToolBatch,
    retired: EndedToolAttempt,
    authorization: RunnerToolAttemptAuthorization,
    source: RunnerLeaseCorrelation,
}

impl RunnerClaimedAttemptReplacement {
    /// Returns the checked successor tool batch.
    pub const fn batch(&self) -> &ToolBatch {
        &self.batch
    }

    /// Returns the physical attempt retired by a claimed retry.
    pub const fn retired(&self) -> &EndedToolAttempt {
        &self.retired
    }

    /// Returns the lost lease correlation that authorized replacement.
    pub const fn source(&self) -> &RunnerLeaseCorrelation {
        &self.source
    }

    /// Returns the fresh replacement attempt correlation.
    pub const fn replacement(&self) -> ToolAttemptDispatchCorrelation {
        self.authorization.authorized.correlation()
    }

    /// Consumes the checked result into its correlated parts.
    pub fn into_parts(self) -> (ToolBatch, EndedToolAttempt, RunnerToolAttemptAuthorization) {
        (self.batch, self.retired, self.authorization)
    }
}

/// Checked successor fence for one lost lease lineage.
#[derive(Debug)]
struct RunnerRetryPreparationGuard(AtomicBool);

impl RunnerRetryPreparationGuard {
    const fn new(preparation: RunnerLeaseRetryPreparation) -> Self {
        Self(AtomicBool::new(matches!(
            preparation,
            RunnerLeaseRetryPreparation::Prepared
        )))
    }

    fn claim(&self) -> bool {
        self.0
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// Single-use retry authority derived from one checked lost lease.
#[derive(Debug)]
pub struct RunnerLeaseRetryAuthority {
    source: RunnerLeaseRetrySource,
    generation: RunnerGeneration,
    claimed_attempt: Option<ToolAttemptId>,
    preparation: RunnerRetryPreparationGuard,
}

// The process-local preparation guard is not part of durable retry identity.
impl PartialEq for RunnerLeaseRetryAuthority {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
            && self.generation == other.generation
            && self.claimed_attempt == other.claimed_attempt
    }
}

impl Eq for RunnerLeaseRetryAuthority {}

impl RunnerLeaseRetryAuthority {
    /// Returns the retry or lease generation.
    pub const fn generation(&self) -> RunnerGeneration {
        self.generation
    }

    /// Reauthorizes the never-executed physical attempt through its owning batch.
    pub fn prepare_unclaimed_attempt(
        &self,
        batch: ToolBatch,
    ) -> Result<RunnerUnclaimedAttemptReauthorization, RunnerDomainError> {
        if self.claimed_attempt.is_some() || !self.preparation.claim() {
            return Err(RunnerDomainError::InvalidState);
        }
        let (batch, mut authorization) = batch
            .reauthorize_unclaimed_runner_attempt(self.source.correlation.dispatch.attempt())
            .map_err(|_| RunnerDomainError::CorrelationMismatch)?;
        if authorization.approved.request().name() != &self.source.correlation.tool
            || authorization.authorized.correlation() != self.source.correlation.dispatch
        {
            return Err(RunnerDomainError::CorrelationMismatch);
        }
        authorization.retry_evidence = Some(RunnerRetryAttemptEvidence::Unclaimed {
            dispatch: self.source.correlation.dispatch,
        });
        Ok(RunnerUnclaimedAttemptReauthorization {
            batch,
            authorization,
        })
    }

    /// Produces a fresh physical attempt through its owning batch.
    pub fn prepare_claimed_attempt(
        &self,
        batch: ToolBatch,
        attempt: ToolAttemptId,
    ) -> Result<RunnerClaimedAttemptReplacement, RunnerDomainError> {
        let claimed = self
            .claimed_attempt
            .ok_or(RunnerDomainError::InvalidState)?;
        if attempt == claimed {
            return Err(RunnerDomainError::AttemptIdentityReuse);
        }
        if !self.preparation.claim() {
            return Err(RunnerDomainError::InvalidState);
        }
        let replacement = batch
            .replace_claimed_attempt(claimed, attempt)
            .map_err(|error| match error.failure() {
                ToolBatchExecutionFailure::AttemptIdentityReuse => {
                    RunnerDomainError::AttemptIdentityReuse
                }
                _ => RunnerDomainError::CorrelationMismatch,
            })?;
        if replacement.approved.request().id() != self.source.correlation.dispatch.request()
            || replacement.approved.request().session()
                != self.source.correlation.dispatch.session()
            || replacement.approved.request().turn() != self.source.correlation.dispatch.turn()
            || replacement.approved.request().name() != &self.source.correlation.tool
            || replacement.retired.session() != self.source.correlation.dispatch.session()
            || replacement.retired.turn() != self.source.correlation.dispatch.turn()
            || replacement.retired.issuing_attempt()
                != self.source.correlation.dispatch.issuing_attempt()
            || replacement.retired.request() != self.source.correlation.dispatch.request()
            || replacement.retired.attempt() != self.source.correlation.dispatch.attempt()
            || replacement.retired.generation() != self.source.correlation.dispatch.generation()
            || replacement.retired.effect_class() != tool_effect_class(self.source.effect)
        {
            return Err(RunnerDomainError::CorrelationMismatch);
        }
        let mut authorization =
            RunnerToolAttemptAuthorization::try_new(replacement.approved, replacement.authorized)?;
        authorization.retry_evidence = Some(RunnerRetryAttemptEvidence::Claimed(
            ClaimedAttemptReplacementEvidence {
                source: self.source.correlation.clone(),
                replacement: authorization.authorized.correlation(),
            },
        ));
        Ok(RunnerClaimedAttemptReplacement {
            batch: replacement.batch,
            retired: replacement.retired,
            authorization,
            source: self.source.correlation.clone(),
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
struct RunnerLeaseRetrySource {
    correlation: RunnerLeaseCorrelation,
    effect: RunnerToolEffectClass,
    credential_authorization: Option<CredentialDispatchAuthorization>,
    state: RunnerLeaseState,
}

impl RunnerLeaseRetrySource {
    fn from_lease(lease: &RunnerLease) -> Self {
        Self {
            correlation: lease.correlation(),
            effect: lease.effect,
            credential_authorization: lease.credential_authorization.clone(),
            state: lease.state,
        }
    }

    fn matches(&self, lease: &RunnerLease) -> bool {
        self.correlation == lease.correlation()
            && self.effect == lease.effect
            && self.credential_authorization == lease.credential_authorization
            && self.state == lease.state
    }
}

/// Working-directory selection at placement.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum WorkingDirectorySelection {
    /// Uses the selected runner default working directory.
    RunnerDefault,
    /// Requires the exact supplied runner working directory.
    Exact(RunnerWorkingDirectory),
}

/// Workspace requirement at placement.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum WorkspaceRequirement {
    /// Requires no runner-provisioned workspace.
    None,
    /// Requires a per-session worktree for the repository key.
    RepositoryWorktree {
        /// The repository for which the runner must provision a worktree.
        repository: WorkspaceRepositoryKey,
    },
}

/// Runner-owned workspace; the runner field is also cleanup ownership.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProvisionedWorkspace {
    /// The owning session identity.
    pub session: SessionId,
    /// The placement revision that owns the workspace.
    pub placement_revision: RunnerGeneration,
    /// The runner responsible for workspace cleanup.
    pub runner: RunnerId,
    /// The repository key for a worktree; absent for a managed private root.
    pub repository: Option<WorkspaceRepositoryKey>,
    /// The canonical clone-URL digest for a worktree; absent for a private root.
    pub canonical_clone_url_digest: Option<CanonicalCloneUrlDigest>,
    /// The profile used to clone a worktree; absent for a private root.
    pub credential_profile: Option<CredentialProfileName>,
    /// The sandbox profile under which the workspace was provisioned.
    pub sandbox: RunnerSandboxProfile,
    /// The runner-interpreted working directory.
    pub working_directory: RunnerWorkingDirectory,
    /// The runner-root-relative path named by the manifest.
    pub relative_path: WorkspaceRelativePath,
    /// The exact canonical workspace-manifest identity.
    pub manifest_id: WorkspaceManifestId,
    /// Git recovery facts for a worktree; absent for a private root.
    pub recovery: Option<WorkspaceRecovery>,
}

/// Complete requested placement axes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRunnerPlacementRequest {
    /// The runner selector that placement must satisfy.
    pub selector: RunnerSelector,
    /// The runner-interpreted working directory.
    pub working_directory: WorkingDirectorySelection,
    /// The requested runner-local credential profile, if any.
    pub credential_profile: Option<CredentialProfileName>,
    /// The workspace capability the placement must provide.
    pub workspace: WorkspaceRequirement,
    /// The explicitly selected sandbox profile.
    pub sandbox: RunnerSandboxProfile,
    /// The exact bounded per-tool permission overrides.
    pub permission_overrides: RunnerToolPermissionOverrides,
}

/// Last credential-grant identity carried by a pinned placement lineage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunnerCredentialGrantLineage {
    /// The runner that issued the retained credential grant.
    pub runner: RunnerId,
    /// The retained credential grant revision.
    pub revision: RunnerGeneration,
}

/// Complete exact pinned facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PinnedRunnerPlacement {
    /// The runner pinned to the session.
    pub runner: RunnerId,
    /// The runner-interpreted working directory.
    pub working_directory: RunnerWorkingDirectory,
    /// The pinned runner-local credential profile, if any.
    pub credential_profile: Option<CredentialProfileName>,
    /// The last credential grant lineage retained by the placement, if any.
    pub grant_lineage: Option<RunnerCredentialGrantLineage>,
    /// The complete tool set admitted by the pin.
    pub tools: BTreeSet<ToolName>,
    /// The registered tools whose locus requires runner execution.
    pub runner_required_tools: BTreeSet<ToolName>,
    /// The workspace provisioned for the pin, if any.
    pub workspace: Option<ProvisionedWorkspace>,
    /// The immutable selected sandbox profile.
    pub sandbox: RunnerSandboxProfile,
    /// The immutable exact per-tool permission overrides.
    pub permission_overrides: RunnerToolPermissionOverrides,
}

/// Durable source of a session placement's runner-loss transition.
///
/// The source is retained so a later replacement transaction can decide
/// whether same-runner recovery is admissible. The current domain replacement
/// transitions refuse a same-runner successor for every source.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RunnerPlacementLossSource {
    /// The runner connection became durably lost.
    Connection,
    /// A current re-registration removed availability required by the pin.
    Registration,
}

/// Exact unpinned identity selection retained after its runner is lost.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunnerLostBeforePin {
    runner: RunnerId,
}

impl RunnerLostBeforePin {
    /// Supplies complete stored facts to placement reconstitution.
    pub const fn from_stored(runner: RunnerId) -> Self {
        Self { runner }
    }

    /// Returns the exact runner selected before initial pinning.
    pub const fn runner(&self) -> RunnerId {
        self.runner
    }
}

/// Exact pinned placement retained after its runner is lost.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LostPinnedRunnerPlacement {
    pinned: PinnedRunnerPlacement,
    source: RunnerPlacementLossSource,
}

impl LostPinnedRunnerPlacement {
    /// Supplies complete stored facts to placement reconstitution.
    pub const fn from_stored(
        pinned: PinnedRunnerPlacement,
        source: RunnerPlacementLossSource,
    ) -> Self {
        Self { pinned, source }
    }

    /// Borrows the complete pinned facts retained by the loss.
    pub const fn pinned(&self) -> &PinnedRunnerPlacement {
        &self.pinned
    }

    /// Returns the durable source of the loss.
    pub const fn source(&self) -> RunnerPlacementLossSource {
        self.source
    }
}

/// Complete retained authority retired by explicit runner abandonment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AbandonedRunnerPlacement {
    /// An exact-identity request was abandoned before first pin.
    BeforePin(RunnerLostBeforePin),
    /// A pinned placement was abandoned after loss.
    Pinned(Box<LostPinnedRunnerPlacement>),
}

/// Session affinity lifecycle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionRunnerPlacementState {
    /// No runner has been pinned for the session.
    Unpinned,
    /// An exact-identity selection lost its runner before initial pinning.
    RunnerLostBeforePin(RunnerLostBeforePin),
    /// The session is pinned to the contained runner facts.
    Pinned(PinnedRunnerPlacement),
    /// The pinned runner is lost and awaits explicit replacement.
    RunnerLost(LostPinnedRunnerPlacement),
    /// Explicit user action terminally retired a lost placement.
    RunnerAbandoned(AbandonedRunnerPlacement),
}

/// Session placement and affinity aggregate.
#[derive(Debug, Eq, PartialEq)]
pub struct SessionRunnerPlacement {
    session: SessionId,
    revision: RunnerGeneration,
    request: SessionRunnerPlacementRequest,
    state: SessionRunnerPlacementState,
}

impl SessionRunnerPlacement {
    /// Creates an unpinned placement for the session request.
    pub const fn new(session: SessionId, request: SessionRunnerPlacementRequest) -> Self {
        Self {
            session,
            revision: RunnerGeneration::one(),
            request,
            state: SessionRunnerPlacementState::Unpinned,
        }
    }

    /// Returns the session placement lifecycle state.
    pub const fn state(&self) -> &SessionRunnerPlacementState {
        &self.state
    }

    /// Returns the session placement revision.
    pub const fn revision(&self) -> RunnerGeneration {
        self.revision
    }

    /// Returns the owning session identity.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the complete placement request.
    pub const fn request(&self) -> &SessionRunnerPlacementRequest {
        &self.request
    }

    /// Pins the selected runner and emits its first fenced lease offer.
    pub fn pin_and_offer_lease(
        mut self,
        enrollment: &RunnerEnrollment,
        registration: &ValidatedRunnerRegistration,
        directory: RunnerWorkingDirectory,
        workspace: Option<ProvisionedWorkspace>,
        authorization: RunnerToolAttemptAuthorization,
        offer: RunnerLeaseOfferRequest,
    ) -> Result<SessionRunnerPin, RunnerDomainError> {
        if self.state != SessionRunnerPlacementState::Unpinned {
            return Err(RunnerDomainError::InvalidState);
        }
        let pinned = validate_placement(
            self.session,
            self.revision,
            &self.request,
            registration,
            directory,
            workspace,
            WorkspaceRevisionMatch::Exact,
        )?;
        let grant = match pinned.credential_profile.clone() {
            Some(profile) => Some(build_grant(
                self.session,
                RunnerGeneration::one(),
                registration,
                profile,
                registration.tool_names().cloned(),
                RunnerApprovalPolicy {
                    sandbox: pinned.sandbox,
                    permission_overrides: &pinned.permission_overrides,
                },
                CredentialProfileGrantState::Active,
            )?),
            None => None,
        };
        self.state = SessionRunnerPlacementState::Pinned(pinned);
        let lease = self.offer_lease(
            enrollment,
            registration,
            grant.as_ref(),
            authorization,
            offer,
        )?;
        Ok(SessionRunnerPin {
            placement: self,
            grant,
            lease,
        })
    }

    /// Offers a new fenced lease from the existing pinned placement.
    pub fn offer_lease(
        &self,
        enrollment: &RunnerEnrollment,
        registration: &ValidatedRunnerRegistration,
        grant: Option<&CredentialProfileGrant>,
        authorization: RunnerToolAttemptAuthorization,
        offer: RunnerLeaseOfferRequest,
    ) -> Result<RunnerLease, RunnerDomainError> {
        let dispatch = validate_dispatch(self, enrollment, registration, grant, &offer.tool)?;
        let (attempt, retry_evidence) = validate_authorized_attempt(
            self.session,
            &offer.tool,
            dispatch.effect,
            dispatch.approval,
            authorization,
        )?;
        if retry_evidence.is_some() {
            return Err(RunnerDomainError::CorrelationMismatch);
        }
        Ok(RunnerLease::offer_validated(ValidatedRunnerLeaseOffer {
            lease: offer.lease,
            dispatch: attempt,
            runner: dispatch.runner,
            tool: offer.tool,
            effect: dispatch.effect,
            credential_authorization: dispatch.credential_authorization,
            generation: RunnerGeneration::one(),
        }))
    }

    /// Offers the checked retry generation for a compatible lost lease.
    pub fn offer_retry(
        &self,
        enrollment: &RunnerEnrollment,
        registration: &ValidatedRunnerRegistration,
        grant: Option<&CredentialProfileGrant>,
        loss: RunnerLeaseLoss,
        authorization: RunnerToolAttemptAuthorization,
    ) -> Result<RunnerLease, RunnerDomainError> {
        let Some((lost, retry)) = loss.into_retry_parts() else {
            return Err(RunnerDomainError::InvalidState);
        };
        if !retry.source.matches(&lost) {
            return Err(RunnerDomainError::CorrelationMismatch);
        }
        let dispatch = validate_dispatch(self, enrollment, registration, grant, &lost.tool)?;
        if lost.dispatch.session() != self.session
            || lost.runner != dispatch.runner
            || lost.effect != dispatch.effect
            || lost.credential_authorization != dispatch.credential_authorization
        {
            return Err(RunnerDomainError::CorrelationMismatch);
        }
        let (attempt, retry_evidence) = validate_authorized_attempt(
            self.session,
            &lost.tool,
            dispatch.effect,
            dispatch.approval,
            authorization,
        )?;
        match (retry.claimed_attempt, retry_evidence) {
            (Some(claimed), _) if attempt.attempt() == claimed => {
                return Err(RunnerDomainError::AttemptIdentityReuse);
            }
            (Some(claimed), Some(RunnerRetryAttemptEvidence::Claimed(replacement)))
                if replacement.source == lost.correlation()
                    && replacement.source.dispatch.attempt() == claimed
                    && replacement.replacement == attempt => {}
            (Some(_), _) => return Err(RunnerDomainError::CorrelationMismatch),
            (None, Some(RunnerRetryAttemptEvidence::Unclaimed { dispatch: source }))
                if attempt == lost.dispatch && source == lost.dispatch => {}
            (None, _) => return Err(RunnerDomainError::CorrelationMismatch),
        }
        Ok(RunnerLease::offer_validated(ValidatedRunnerLeaseOffer {
            lease: lost.lease,
            dispatch: attempt,
            runner: lost.runner,
            tool: lost.tool,
            effect: lost.effect,
            credential_authorization: lost.credential_authorization,
            generation: retry.generation,
        }))
    }

    /// Marks the currently pinned runner as lost.
    pub fn mark_runner_lost(mut self) -> Result<Self, RunnerDomainError> {
        let SessionRunnerPlacementState::Pinned(pinned) = self.state else {
            return Err(RunnerDomainError::InvalidState);
        };
        self.state = SessionRunnerPlacementState::RunnerLost(LostPinnedRunnerPlacement {
            pinned,
            source: RunnerPlacementLossSource::Connection,
        });
        Ok(self)
    }

    /// Marks an exact-identity selection lost before its initial pin.
    pub fn mark_runner_lost_before_pin(
        mut self,
        runner: RunnerId,
    ) -> Result<Self, RunnerDomainError> {
        match (&self.state, &self.request.selector) {
            (SessionRunnerPlacementState::Unpinned, RunnerSelector::Identity(selected))
                if *selected == runner => {}
            (
                SessionRunnerPlacementState::Unpinned,
                RunnerSelector::Identity(_) | RunnerSelector::CapabilityClass(_),
            )
            | (
                SessionRunnerPlacementState::Pinned(_)
                | SessionRunnerPlacementState::RunnerLostBeforePin(_)
                | SessionRunnerPlacementState::RunnerLost(_)
                | SessionRunnerPlacementState::RunnerAbandoned(_),
                _,
            ) => return Err(RunnerDomainError::InvalidState),
        }
        self.state =
            SessionRunnerPlacementState::RunnerLostBeforePin(RunnerLostBeforePin { runner });
        Ok(self)
    }

    /// Marks the runner lost when its current registration no longer supports the pin.
    pub fn reconcile_registration(
        mut self,
        registration: &ValidatedRunnerRegistration,
    ) -> Result<Self, RunnerDomainError> {
        let SessionRunnerPlacementState::Pinned(pinned) = &self.state else {
            return Err(RunnerDomainError::InvalidState);
        };
        if !registration.is_current() {
            return Err(RunnerDomainError::RegistrationChanged);
        }
        if registration.runner != pinned.runner {
            return Err(RunnerDomainError::CorrelationMismatch);
        }
        if registration_preserves_snapshot(&self.request, pinned, registration) {
            return Ok(self);
        }
        let SessionRunnerPlacementState::Pinned(pinned) = self.state else {
            return Err(RunnerDomainError::InvalidState);
        };
        self.state = SessionRunnerPlacementState::RunnerLost(LostPinnedRunnerPlacement {
            pinned,
            source: RunnerPlacementLossSource::Registration,
        });
        Ok(self)
    }

    /// Replaces an exact runner lost before pinning without fabricating pinned facts.
    pub fn replace_lost_runner_before_pin(
        self,
        request: SessionRunnerPlacementRequest,
        registration: &ValidatedRunnerRegistration,
    ) -> Result<RunnerPrePinReplacement, RunnerDomainError> {
        let before = match self.state {
            SessionRunnerPlacementState::RunnerLostBeforePin(before) => before,
            SessionRunnerPlacementState::Unpinned
            | SessionRunnerPlacementState::Pinned(_)
            | SessionRunnerPlacementState::RunnerLost(_)
            | SessionRunnerPlacementState::RunnerAbandoned(_) => {
                return Err(RunnerDomainError::InvalidState);
            }
        };
        match &request.selector {
            RunnerSelector::Identity(runner) if *runner == registration.runner => {}
            RunnerSelector::Identity(_) | RunnerSelector::CapabilityClass(_) => {
                return Err(RunnerDomainError::CorrelationMismatch);
            }
        }
        validate_placement_request(&request, registration)?;
        if registration.runner == before.runner {
            return Err(RunnerDomainError::CorrelationMismatch);
        }
        let revision = self
            .revision
            .checked_next()
            .ok_or(RunnerDomainError::GenerationExhausted)?;
        Ok(RunnerPrePinReplacement {
            placement: Self {
                session: self.session,
                revision,
                request: request.clone(),
                state: SessionRunnerPlacementState::Unpinned,
            },
            before,
            prior_request: self.request,
            replacement_request: request,
        })
    }

    /// Replaces a lost runner while preserving explicit placement and grant changes.
    pub fn replace_lost_runner(
        self,
        request: SessionRunnerPlacementRequest,
        registration: &ValidatedRunnerRegistration,
        directory: RunnerWorkingDirectory,
        workspace: Option<ProvisionedWorkspace>,
        prior_grant: Option<CredentialProfileGrant>,
    ) -> Result<RunnerPlacementReplacement, RunnerDomainError> {
        let lost = match self.state {
            SessionRunnerPlacementState::RunnerLost(lost) => lost,
            SessionRunnerPlacementState::Unpinned
            | SessionRunnerPlacementState::Pinned(_)
            | SessionRunnerPlacementState::RunnerLostBeforePin(_)
            | SessionRunnerPlacementState::RunnerAbandoned(_) => {
                return Err(RunnerDomainError::InvalidState);
            }
        };
        let before = lost.pinned;
        if !registration.is_current() {
            return Err(RunnerDomainError::RegistrationChanged);
        }
        if registration.runner == before.runner {
            return Err(RunnerDomainError::CorrelationMismatch);
        }
        let revision = self
            .revision
            .checked_next()
            .ok_or(RunnerDomainError::GenerationExhausted)?;
        let mut after = validate_placement(
            self.session,
            revision,
            &request,
            registration,
            directory,
            workspace,
            WorkspaceRevisionMatch::Exact,
        )?;
        let prior_request = self.request;
        let (grant, grant_change) =
            successor_grant(self.session, &before, &after, registration, prior_grant)?;
        after.grant_lineage = grant.as_ref().map(|grant| RunnerCredentialGrantLineage {
            runner: grant.runner,
            revision: grant.revision,
        });
        Ok(RunnerPlacementReplacement {
            placement: Self {
                session: self.session,
                revision,
                request: request.clone(),
                state: SessionRunnerPlacementState::Pinned(after.clone()),
            },
            change: RunnerPlacementChange {
                session: self.session,
                prior_revision: self.revision,
                replacement_revision: revision,
                before_request: prior_request,
                after_request: request,
                before,
                after,
            },
            grant,
            grant_change,
        })
    }

    /// Terminally abandons the exact current lost placement.
    pub fn abandon_lost_runner(mut self) -> Result<Self, RunnerDomainError> {
        self.state = match self.state {
            SessionRunnerPlacementState::RunnerLostBeforePin(lost) => {
                SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::BeforePin(
                    lost,
                ))
            }
            SessionRunnerPlacementState::RunnerLost(lost) => {
                SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::Pinned(
                    Box::new(lost),
                ))
            }
            SessionRunnerPlacementState::Unpinned
            | SessionRunnerPlacementState::Pinned(_)
            | SessionRunnerPlacementState::RunnerAbandoned(_) => {
                return Err(RunnerDomainError::InvalidState);
            }
        };
        Ok(self)
    }

    /// Replaces the pinned credential profile and advances the placement revision.
    pub fn replace_credential_profile(
        self,
        grant: CredentialProfileGrant,
        registration: &ValidatedRunnerRegistration,
        profile: CredentialProfileName,
        tools: impl IntoIterator<Item = ToolName>,
    ) -> Result<CredentialProfilePlacementReplacement, RunnerDomainError> {
        let SessionRunnerPlacementState::Pinned(before) = self.state else {
            return Err(RunnerDomainError::InvalidState);
        };
        if !registration.is_current() {
            return Err(RunnerDomainError::RegistrationChanged);
        }
        let Some(current_profile) = &before.credential_profile else {
            return Err(RunnerDomainError::CredentialProfileUnavailable);
        };
        if !grant.matches_selection(self.session, before.runner, current_profile)
            || before.grant_lineage != Some(grant.lineage())
        {
            return Err(RunnerDomainError::CorrelationMismatch);
        }
        if registration.runner != before.runner {
            return Err(RunnerDomainError::CorrelationMismatch);
        }
        if !registration_preserves_snapshot(&self.request, &before, registration) {
            return Err(RunnerDomainError::RegistrationChanged);
        }
        if before.workspace.as_ref().is_some_and(|workspace| {
            workspace.repository.is_some()
                && workspace.credential_profile.as_ref() != Some(&profile)
        }) {
            return Err(RunnerDomainError::CredentialProfileUnavailable);
        }
        let revision = self
            .revision
            .checked_next()
            .ok_or(RunnerDomainError::GenerationExhausted)?;
        let grant = grant.replace_for(
            registration,
            profile.clone(),
            tools,
            before.sandbox,
            &before.permission_overrides,
        )?;
        let mut after = before.clone();
        after.credential_profile = Some(profile.clone());
        after.grant_lineage = Some(RunnerCredentialGrantLineage {
            runner: grant.grant.runner,
            revision: grant.grant.revision,
        });
        let before_request = self.request.clone();
        let mut request = self.request;
        request.credential_profile = Some(profile);
        Ok(CredentialProfilePlacementReplacement {
            placement: Self {
                session: self.session,
                revision,
                request: request.clone(),
                state: SessionRunnerPlacementState::Pinned(after.clone()),
            },
            placement_change: RunnerPlacementChange {
                session: self.session,
                prior_revision: self.revision,
                replacement_revision: revision,
                before_request,
                after_request: request.clone(),
                before,
                after,
            },
            grant,
        })
    }

    /// Reconstitutes placement state against registration and retained grant lineage.
    pub fn reconstitute(
        input: SessionRunnerPlacementReconstitutionInput,
        expected_session: SessionId,
        registration: Option<&ValidatedRunnerRegistration>,
        profileless_tombstone: Option<&CredentialProfileGrant>,
    ) -> Result<Self, RunnerDomainError> {
        if input.session != expected_session {
            return Err(RunnerDomainError::CorruptStoredFacts);
        }
        let placement = Self {
            session: input.session,
            revision: input.revision,
            request: input.request,
            state: input.state,
        };
        let history = input.history;
        let reconstituted_state = placement.state.clone();
        match reconstituted_state {
            SessionRunnerPlacementState::Unpinned
                if placement_revision_history_matches(
                    placement.revision,
                    &placement.request,
                    &history,
                ) =>
            {
                Ok(placement)
            }
            SessionRunnerPlacementState::RunnerLostBeforePin(lost)
            | SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::BeforePin(
                lost,
            )) if selector_names_runner(&placement.request.selector, lost.runner)
                && placement_revision_history_matches(
                    placement.revision,
                    &placement.request,
                    &history,
                ) =>
            {
                Ok(placement)
            }
            SessionRunnerPlacementState::Pinned(stored) => reconstitute_pinned_placement(
                placement,
                stored,
                registration,
                profileless_tombstone,
            ),
            SessionRunnerPlacementState::RunnerLost(lost) => reconstitute_pinned_placement(
                placement,
                lost.pinned,
                registration,
                profileless_tombstone,
            ),
            SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::Pinned(
                lost,
            )) => reconstitute_pinned_placement(
                placement,
                lost.pinned,
                registration,
                profileless_tombstone,
            ),
            SessionRunnerPlacementState::Unpinned
            | SessionRunnerPlacementState::RunnerLostBeforePin(_)
            | SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::BeforePin(
                _,
            )) => Err(RunnerDomainError::CorruptStoredFacts),
        }
    }
}

fn selector_names_runner(selector: &RunnerSelector, runner: RunnerId) -> bool {
    match selector {
        RunnerSelector::Identity(selected) => *selected == runner,
        RunnerSelector::CapabilityClass(_) => false,
    }
}

fn reconstitute_pinned_placement(
    placement: SessionRunnerPlacement,
    stored: PinnedRunnerPlacement,
    registration: Option<&ValidatedRunnerRegistration>,
    profileless_tombstone: Option<&CredentialProfileGrant>,
) -> Result<SessionRunnerPlacement, RunnerDomainError> {
    let pinned_registration = registration.ok_or(RunnerDomainError::CorruptStoredFacts)?;
    let mut checked = validate_placement(
        placement.session,
        placement.revision,
        &placement.request,
        pinned_registration,
        stored.working_directory.clone(),
        stored.workspace.clone(),
        WorkspaceRevisionMatch::Retained,
    )?;
    checked.grant_lineage = stored.grant_lineage;
    let lineage_is_valid = match (
        stored.credential_profile.as_ref(),
        stored.grant_lineage,
        profileless_tombstone,
    ) {
        (Some(_), Some(lineage), None) => {
            lineage.runner == stored.runner && lineage.revision <= placement.revision
        }
        (None, None, None) => true,
        (None, Some(lineage), Some(tombstone)) => {
            let tombstone_is_revoked = match tombstone.state {
                CredentialProfileGrantState::Active => false,
                CredentialProfileGrantState::Revoked => true,
            };
            tombstone.session == placement.session
                && tombstone_is_revoked
                && tombstone.lineage() == lineage
                && lineage.revision <= placement.revision
        }
        (None, None, Some(_))
        | (Some(_), None, _)
        | (Some(_), Some(_), Some(_))
        | (None, Some(_), None) => false,
    };
    if lineage_is_valid && checked == stored {
        Ok(placement)
    } else {
        Err(RunnerDomainError::CorruptStoredFacts)
    }
}

fn placement_revision_history_matches(
    mut revision: RunnerGeneration,
    request: &SessionRunnerPlacementRequest,
    history: &RunnerPlacementReconstitutionHistory,
) -> bool {
    let mut request = request;
    let replacements = match history {
        RunnerPlacementReconstitutionHistory::Initial => {
            return revision == RunnerGeneration::one();
        }
        RunnerPlacementReconstitutionHistory::PrePinReplacements(replacements)
            if !replacements.is_empty() =>
        {
            replacements
        }
        RunnerPlacementReconstitutionHistory::PrePinReplacements(_) => return false,
    };
    for replacement in replacements.iter().rev() {
        let successor_differs = match request.selector {
            RunnerSelector::Identity(successor) => successor != replacement.lost_runner,
            RunnerSelector::CapabilityClass(_) => false,
        };
        let predecessor_names_loss = match replacement.prior_request.selector {
            RunnerSelector::Identity(prior) => prior == replacement.lost_runner,
            RunnerSelector::CapabilityClass(_) => false,
        };
        if replacement.prior_revision.checked_next() != Some(revision)
            || !predecessor_names_loss
            || &replacement.replacement_request != request
            || !successor_differs
        {
            return false;
        }
        revision = replacement.prior_revision;
        request = &replacement.prior_request;
    }
    revision == RunnerGeneration::one()
}

/// Append-only history proof used when reconstituting a placement revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunnerPlacementReconstitutionHistory {
    /// Revision one was created directly from session placement intent.
    Initial,
    /// Chronological nonempty lost-before-pin replacement history.
    PrePinReplacements(Vec<RunnerPrePinReplacementHistory>),
}

/// One stack-safe element of append-only pre-pin replacement history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerPrePinReplacementHistory {
    /// Exact predecessor revision consumed by the replacement.
    pub prior_revision: RunnerGeneration,
    /// Exact runner retained by the predecessor loss.
    pub lost_runner: RunnerId,
    /// Complete predecessor request retained by append-only history.
    pub prior_request: SessionRunnerPlacementRequest,
    /// Complete successor request installed by append-only history.
    pub replacement_request: SessionRunnerPlacementRequest,
}

/// Complete placement facts loaded from one canonical durable revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRunnerPlacementReconstitutionInput {
    /// The owning session identity.
    pub session: SessionId,
    /// The stored placement revision.
    pub revision: RunnerGeneration,
    /// The complete placement request.
    pub request: SessionRunnerPlacementRequest,
    /// The stored domain state.
    pub state: SessionRunnerPlacementState,
    /// The append-only transition that makes the stored revision reachable.
    pub history: RunnerPlacementReconstitutionHistory,
}

struct ValidatedRunnerDispatch {
    runner: RunnerId,
    approval: CredentialToolApproval,
    effect: RunnerToolEffectClass,
    credential_authorization: Option<CredentialDispatchAuthorization>,
}

fn validate_dispatch(
    placement: &SessionRunnerPlacement,
    enrollment: &RunnerEnrollment,
    registration: &ValidatedRunnerRegistration,
    grant: Option<&CredentialProfileGrant>,
    tool: &ToolName,
) -> Result<ValidatedRunnerDispatch, RunnerDomainError> {
    let SessionRunnerPlacementState::Pinned(pinned) = &placement.state else {
        return Err(RunnerDomainError::InvalidState);
    };
    enrollment.authorizes(registration)?;
    if !registration_preserves_snapshot(&placement.request, pinned, registration) {
        return Err(RunnerDomainError::RegistrationChanged);
    }
    if !pinned.tools.contains(tool) {
        return Err(RunnerDomainError::ToolUnavailable);
    }
    let declaration = registration
        .tool(tool)
        .ok_or(RunnerDomainError::ToolUnavailable)?;
    let credential_authorization = match (&pinned.credential_profile, grant) {
        (None, None) => None,
        (Some(profile), Some(grant)) if pinned.grant_lineage == Some(grant.lineage()) => {
            Some(grant.authorization_for(placement.session, pinned.runner, profile, tool)?)
        }
        _ => return Err(RunnerDomainError::CredentialProfileUnavailable),
    };
    let approval = resolve_runner_approval(
        declaration.effect,
        pinned.sandbox,
        &pinned.permission_overrides,
        tool,
    );
    if credential_authorization
        .as_ref()
        .is_some_and(|authorization| authorization.approval != approval)
    {
        return Err(RunnerDomainError::CorrelationMismatch);
    }
    Ok(ValidatedRunnerDispatch {
        runner: pinned.runner,
        approval,
        effect: declaration.effect,
        credential_authorization,
    })
}

/// Whether the user confirmed this exact request in advance, through either
/// the applied user command that decided it or the one-shot user override
/// recorded against the delegate denial the request re-proposes. Both are
/// per-request user agency exercised before dispatch. The frozen session
/// blanket is excluded: it is standing daemon-local automation and never
/// runner-dispatch authority.
const fn confirmed_by_user(source: ToolDecisionSource) -> bool {
    matches!(
        source,
        ToolDecisionSource::UserCommand | ToolDecisionSource::UserOverride
    )
}

fn validate_authorized_attempt(
    session: SessionId,
    tool: &ToolName,
    effect: RunnerToolEffectClass,
    approval: CredentialToolApproval,
    authorization: RunnerToolAttemptAuthorization,
) -> Result<
    (
        ToolAttemptDispatchCorrelation,
        Option<RunnerRetryAttemptEvidence>,
    ),
    RunnerDomainError,
> {
    let (approved, authorized, retry_evidence) = authorization.into_parts();
    let (attempt, correlation) = authorized.into_parts();
    let expected_effect = tool_effect_class(effect);
    if approved.request().name() != tool
        || approved.request().id() != correlation.request()
        || approved.request().session() != session
        || approved.request().turn() != correlation.turn()
        || attempt.session() != session
        || attempt.effect_class() != expected_effect
        || attempt.attempt() != correlation.attempt()
        || approved.approval().source() == ToolDecisionSource::SessionBlanket
        || (approval == CredentialToolApproval::SessionPolicy
            && !confirmed_by_user(approved.approval().source()))
    {
        return Err(RunnerDomainError::CorrelationMismatch);
    }
    Ok((correlation, retry_evidence))
}

const fn tool_effect_class(effect: RunnerToolEffectClass) -> ToolEffectClass {
    match effect {
        RunnerToolEffectClass::Pure => ToolEffectClass::EffectFree,
        RunnerToolEffectClass::Idempotent | RunnerToolEffectClass::SideEffecting => {
            ToolEffectClass::ExternalEffect
        }
    }
}

fn registration_preserves_snapshot(
    request: &SessionRunnerPlacementRequest,
    pinned: &PinnedRunnerPlacement,
    registration: &ValidatedRunnerRegistration,
) -> bool {
    pinned.runner == registration.runner
        && pinned.sandbox == request.sandbox
        && pinned.permission_overrides == request.permission_overrides
        && registration.satisfies(&request.selector)
        && registration.supports_sandbox(request.sandbox)
        && pinned
            .runner_required_tools
            .iter()
            .all(|tool| registration.tool(tool).is_some())
        && pinned
            .credential_profile
            .as_ref()
            .is_none_or(|profile| registration.profile(profile).is_some())
        && match &request.workspace {
            WorkspaceRequirement::None => true,
            WorkspaceRequirement::RepositoryWorktree { repository } => {
                registration.supports_workspace(WorkspaceCapability::WorktreePerSession)
                    && registration.repository(repository).is_some_and(|entry| {
                        entry.credential_profile() == request.credential_profile.as_ref()
                    })
            }
        }
}

#[derive(Clone, Copy)]
enum WorkspaceRevisionMatch {
    Exact,
    Retained,
}

fn validate_placement(
    session: SessionId,
    revision: RunnerGeneration,
    request: &SessionRunnerPlacementRequest,
    registration: &ValidatedRunnerRegistration,
    directory: RunnerWorkingDirectory,
    workspace: Option<ProvisionedWorkspace>,
    workspace_revision_match: WorkspaceRevisionMatch,
) -> Result<PinnedRunnerPlacement, RunnerDomainError> {
    validate_placement_request_against(request, registration)?;
    if let WorkingDirectorySelection::Exact(required) = &request.working_directory
        && required != &directory
    {
        return Err(RunnerDomainError::WorkingDirectoryMismatch);
    }
    let common_workspace_facts_match = |actual: &ProvisionedWorkspace| {
        let terminal = if actual.repository.is_some() {
            "repo"
        } else {
            "work"
        };
        let expected_relative_path = format!(
            "sessions/{}/{}/{}",
            actual.session.as_uuid(),
            actual.placement_revision.get(),
            terminal,
        );
        actual.session == session
            && match workspace_revision_match {
                WorkspaceRevisionMatch::Exact => actual.placement_revision == revision,
                WorkspaceRevisionMatch::Retained => actual.placement_revision <= revision,
            }
            && actual.runner == registration.runner
            && actual.sandbox == request.sandbox
            && actual.working_directory == directory
            && actual.relative_path.as_str() == expected_relative_path
    };
    match (&request.workspace, &workspace) {
        (WorkspaceRequirement::None, None)
            if matches!(
                request.working_directory,
                WorkingDirectorySelection::Exact(_) | WorkingDirectorySelection::RunnerDefault
            ) && (request.sandbox == RunnerSandboxProfile::Ambient
                || matches!(
                    request.working_directory,
                    WorkingDirectorySelection::Exact(_)
                )) => {}
        (WorkspaceRequirement::None, Some(actual))
            if request.sandbox == RunnerSandboxProfile::WorkspaceRestricted
                && common_workspace_facts_match(actual)
                && actual.repository.is_none()
                && actual.canonical_clone_url_digest.is_none()
                && actual.credential_profile.is_none()
                && actual.recovery.is_none()
                && matches!(
                    request.working_directory,
                    WorkingDirectorySelection::RunnerDefault
                ) => {}
        (WorkspaceRequirement::RepositoryWorktree { repository }, Some(actual))
            if registration.supports_workspace(WorkspaceCapability::WorktreePerSession)
                && common_workspace_facts_match(actual)
                && actual.repository.as_ref() == Some(repository)
                && actual.canonical_clone_url_digest.is_some()
                && actual.credential_profile.as_ref() == request.credential_profile.as_ref()
                && actual.recovery.is_some() => {}
        (WorkspaceRequirement::RepositoryWorktree { .. }, _)
            if !registration.supports_workspace(WorkspaceCapability::WorktreePerSession) =>
        {
            return Err(RunnerDomainError::WorkspaceCapabilityUnavailable);
        }
        _ => return Err(RunnerDomainError::WorkspaceMismatch),
    }
    Ok(PinnedRunnerPlacement {
        runner: registration.runner,
        working_directory: directory,
        credential_profile: request.credential_profile.clone(),
        grant_lineage: request
            .credential_profile
            .as_ref()
            .map(|_| RunnerCredentialGrantLineage {
                runner: registration.runner,
                revision: RunnerGeneration::one(),
            }),
        tools: registration.tools.keys().cloned().collect(),
        runner_required_tools: registration
            .tools
            .iter()
            .filter(|(_, declaration)| {
                matches!(declaration.loci, ToolAdmissibleLoci::RunnerOnly { .. })
            })
            .map(|(tool, _)| tool.clone())
            .collect(),
        workspace,
        sandbox: request.sandbox,
        permission_overrides: request.permission_overrides.clone(),
    })
}

fn validate_placement_request(
    request: &SessionRunnerPlacementRequest,
    registration: &ValidatedRunnerRegistration,
) -> Result<(), RunnerDomainError> {
    if !registration.is_current() {
        return Err(RunnerDomainError::RegistrationChanged);
    }
    validate_placement_request_against(request, registration)
}

fn validate_placement_request_against(
    request: &SessionRunnerPlacementRequest,
    registration: &ValidatedRunnerRegistration,
) -> Result<(), RunnerDomainError> {
    if !registration.satisfies(&request.selector) {
        return Err(RunnerDomainError::SelectorMismatch);
    }
    if !registration.supports_sandbox(request.sandbox) {
        return Err(RunnerDomainError::SandboxProfileUnavailable);
    }
    if let Some((tool, _)) = request
        .permission_overrides
        .iter()
        .find(|(tool, _)| !registration.catalog_tools.contains(tool))
    {
        return Err(RunnerDomainError::ToolUndeclared(tool.clone()));
    }
    if request
        .credential_profile
        .as_ref()
        .is_some_and(|profile| registration.profile(profile).is_none())
    {
        return Err(RunnerDomainError::CredentialProfileUnavailable);
    }
    match &request.workspace {
        WorkspaceRequirement::None => {}
        WorkspaceRequirement::RepositoryWorktree { repository } => {
            if !registration.supports_workspace(WorkspaceCapability::WorktreePerSession) {
                return Err(RunnerDomainError::WorkspaceCapabilityUnavailable);
            }
            let entry = registration
                .repository(repository)
                .ok_or(RunnerDomainError::RepositoryUnavailable)?;
            if entry.credential_profile() != request.credential_profile.as_ref() {
                return Err(RunnerDomainError::CredentialProfileUnavailable);
            }
        }
    }
    Ok(())
}

/// Successful first pin with its optional runner-bound credential grant.
#[derive(Debug, Eq, PartialEq)]
pub struct SessionRunnerPin {
    /// The resulting session runner placement.
    pub placement: SessionRunnerPlacement,
    /// The resulting credential grant, when the selection requires one.
    pub grant: Option<CredentialProfileGrant>,
    /// The initial lease emitted by the pin.
    pub lease: RunnerLease,
}

/// Successful replacement of an exact runner lost before the first pin.
#[derive(Debug, Eq, PartialEq)]
pub struct RunnerPrePinReplacement {
    /// The successor unpinned placement at the next positive revision.
    pub placement: SessionRunnerPlacement,
    /// The exact lost identity consumed by replacement.
    pub before: RunnerLostBeforePin,
    /// The complete request retained by the loss.
    pub prior_request: SessionRunnerPlacementRequest,
    /// The complete successor request installed by replacement.
    pub replacement_request: SessionRunnerPlacementRequest,
}

/// Successful explicit placement replacement.
#[derive(Debug, Eq, PartialEq)]
pub struct RunnerPlacementReplacement {
    /// The resulting session runner placement.
    pub placement: SessionRunnerPlacement,
    /// The complete before-and-after change facts.
    pub change: RunnerPlacementChange,
    /// The resulting credential grant, when the selection requires one.
    pub grant: Option<CredentialProfileGrant>,
    /// The complete credential grant change, when replacement changed one.
    pub grant_change: Option<RunnerCredentialGrantChange>,
}

/// Complete before-and-after facts for runner placement replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerPlacementChange {
    /// The owning session identity.
    pub session: SessionId,
    /// The placement or grant revision before replacement.
    pub prior_revision: RunnerGeneration,
    /// The placement or grant revision after replacement.
    pub replacement_revision: RunnerGeneration,
    /// The placement request before replacement.
    pub before_request: SessionRunnerPlacementRequest,
    /// The placement request after replacement.
    pub after_request: SessionRunnerPlacementRequest,
    /// The pinned placement before replacement.
    pub before: PinnedRunnerPlacement,
    /// The pinned placement after replacement.
    pub after: PinnedRunnerPlacement,
}

/// One explicit profile/grant replacement bound to pinned placement.
#[derive(Debug, Eq, PartialEq)]
pub struct CredentialProfilePlacementReplacement {
    /// The resulting session runner placement.
    pub placement: SessionRunnerPlacement,
    /// The complete placement change accompanying the grant replacement.
    pub placement_change: RunnerPlacementChange,
    /// The replacement credential grant and its change facts.
    pub grant: CredentialProfileGrantReplacement,
}

/// Active or terminally revoked credential grant.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CredentialProfileGrantState {
    /// The credential profile grant may authorize tool dispatch.
    Active,
    /// The credential profile grant is terminally revoked.
    Revoked,
}

/// Daemon grant snapshot for one runner-local profile.
#[derive(Debug, Eq, PartialEq)]
pub struct CredentialProfileGrant {
    session: SessionId,
    runner: RunnerId,
    revision: RunnerGeneration,
    profile: CredentialProfileName,
    tools: BTreeSet<ToolName>,
    approvals: BTreeMap<ToolName, CredentialToolApproval>,
    state: CredentialProfileGrantState,
}

impl CredentialProfileGrant {
    fn matches_binding(
        &self,
        session: SessionId,
        runner: RunnerId,
        profile: &CredentialProfileName,
    ) -> bool {
        self.session == session && self.runner == runner && &self.profile == profile
    }

    fn matches_selection(
        &self,
        session: SessionId,
        runner: RunnerId,
        profile: &CredentialProfileName,
    ) -> bool {
        self.state == CredentialProfileGrantState::Active
            && self.matches_binding(session, runner, profile)
    }

    /// Returns the credential grant lifecycle state.
    pub const fn state(&self) -> CredentialProfileGrantState {
        self.state
    }

    /// Returns the credential grant revision.
    pub const fn revision(&self) -> RunnerGeneration {
        self.revision
    }

    /// Returns the exact runner and revision that issued the grant.
    pub const fn lineage(&self) -> RunnerCredentialGrantLineage {
        RunnerCredentialGrantLineage {
            runner: self.runner,
            revision: self.revision,
        }
    }

    /// Returns the runner-local profile named by this grant.
    pub const fn profile(&self) -> &CredentialProfileName {
        &self.profile
    }

    /// Returns the owning session identity.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the runner identity.
    pub const fn runner(&self) -> RunnerId {
        self.runner
    }

    /// Iterates the tools authorized by this grant.
    pub fn tools(&self) -> impl Iterator<Item = &ToolName> {
        self.tools.iter()
    }

    /// Iterates the explicit tool approval overrides.
    pub fn approvals(&self) -> impl Iterator<Item = (&ToolName, CredentialToolApproval)> {
        self.approvals
            .iter()
            .map(|(tool, approval)| (tool, *approval))
    }

    fn reconstitution_facts(&self) -> CredentialProfileGrantReconstitutionInput {
        CredentialProfileGrantReconstitutionInput {
            session: self.session,
            runner: self.runner,
            revision: self.revision,
            profile: self.profile.clone(),
            tools: self.tools.clone(),
            approvals: self.approvals.clone(),
            state: self.state,
        }
    }

    fn authorization_for(
        &self,
        session: SessionId,
        runner: RunnerId,
        profile: &CredentialProfileName,
        tool: &ToolName,
    ) -> Result<CredentialDispatchAuthorization, RunnerDomainError> {
        if self.state != CredentialProfileGrantState::Active {
            return Err(RunnerDomainError::GrantRevoked);
        }
        if self.session != session
            || self.runner != runner
            || &self.profile != profile
            || !self.tools.contains(tool)
        {
            return Err(RunnerDomainError::ToolUnavailable);
        }
        Ok(CredentialDispatchAuthorization {
            session: self.session,
            runner: self.runner,
            grant_revision: self.revision,
            profile: self.profile.clone(),
            tool: tool.clone(),
            approval: self.approvals[tool],
        })
    }

    fn replace_for(
        self,
        registration: &ValidatedRunnerRegistration,
        profile: CredentialProfileName,
        tools: impl IntoIterator<Item = ToolName>,
        sandbox: RunnerSandboxProfile,
        permission_overrides: &RunnerToolPermissionOverrides,
    ) -> Result<CredentialProfileGrantReplacement, RunnerDomainError> {
        if self.state != CredentialProfileGrantState::Active {
            return Err(RunnerDomainError::InvalidState);
        }
        if self.runner != registration.runner {
            return Err(RunnerDomainError::CorrelationMismatch);
        }
        let revision = self
            .revision
            .checked_next()
            .ok_or(RunnerDomainError::GenerationExhausted)?;
        let replacement = build_grant(
            self.session,
            revision,
            registration,
            profile,
            tools,
            RunnerApprovalPolicy {
                sandbox,
                permission_overrides,
            },
            CredentialProfileGrantState::Active,
        )?;
        Ok(CredentialProfileGrantReplacement {
            change: CredentialProfileChange {
                session: self.session,
                prior_revision: self.revision,
                replacement_revision: revision,
                before_profile: self.profile,
                after_profile: replacement.profile.clone(),
                before_tools: self.tools,
                after_tools: replacement.tools.clone(),
            },
            grant: replacement,
        })
    }

    /// Transitions the value to its terminal revoked state.
    pub fn revoke(mut self) -> Result<Self, RunnerDomainError> {
        if self.state != CredentialProfileGrantState::Active {
            return Err(RunnerDomainError::InvalidState);
        }
        self.state = CredentialProfileGrantState::Revoked;
        Ok(self)
    }

    /// Reconstitutes a credential grant against its expected session and registration.
    pub fn reconstitute(
        input: CredentialProfileGrantReconstitutionInput,
        expected_session: SessionId,
        registration: &ValidatedRunnerRegistration,
        sandbox: RunnerSandboxProfile,
        permission_overrides: &RunnerToolPermissionOverrides,
    ) -> Result<Self, RunnerDomainError> {
        if input.session != expected_session {
            return Err(RunnerDomainError::CorruptStoredFacts);
        }
        let checked = build_grant(
            input.session,
            input.revision,
            registration,
            input.profile,
            input.tools,
            RunnerApprovalPolicy {
                sandbox,
                permission_overrides,
            },
            input.state,
        )?;
        if checked.runner == input.runner && checked.approvals == input.approvals {
            Ok(checked)
        } else {
            Err(RunnerDomainError::CorruptStoredFacts)
        }
    }
}

/// Complete credential-grant facts loaded from canonical storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialProfileGrantReconstitutionInput {
    /// The owning session identity.
    pub session: SessionId,
    /// The runner that issued the stored grant.
    pub runner: RunnerId,
    /// The stored credential grant revision.
    pub revision: RunnerGeneration,
    /// The runner-local credential profile name.
    pub profile: CredentialProfileName,
    /// The exact tools authorized by the stored grant.
    pub tools: BTreeSet<ToolName>,
    /// The exact per-tool approval postures.
    pub approvals: BTreeMap<ToolName, CredentialToolApproval>,
    /// The stored domain state.
    pub state: CredentialProfileGrantState,
}

/// Complete before-and-after credential-grant facts from runner replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerCredentialGrantChange {
    /// The complete credential grant before replacement, if one existed.
    pub before: Option<CredentialProfileGrantReconstitutionInput>,
    /// The complete credential grant after replacement, if one exists.
    pub after: Option<CredentialProfileGrantReconstitutionInput>,
}

fn successor_grant(
    session: SessionId,
    before: &PinnedRunnerPlacement,
    after: &PinnedRunnerPlacement,
    registration: &ValidatedRunnerRegistration,
    prior: Option<CredentialProfileGrant>,
) -> Result<
    (
        Option<CredentialProfileGrant>,
        Option<RunnerCredentialGrantChange>,
    ),
    RunnerDomainError,
> {
    let prior = match (
        before.credential_profile.as_ref(),
        before.grant_lineage,
        prior,
    ) {
        (Some(profile), Some(lineage), Some(prior))
            if prior.matches_binding(session, before.runner, profile)
                && prior.lineage() == lineage =>
        {
            Some(prior)
        }
        (None, Some(lineage), Some(prior))
            if prior.session == session
                && prior.state == CredentialProfileGrantState::Revoked
                && prior.lineage() == lineage =>
        {
            Some(prior)
        }
        (None, None, None) => None,
        _ => return Err(RunnerDomainError::CorrelationMismatch),
    };
    let before_facts = prior
        .as_ref()
        .map(CredentialProfileGrant::reconstitution_facts);
    let grant = match prior {
        None => after
            .credential_profile
            .clone()
            .map(|profile| {
                build_grant(
                    session,
                    RunnerGeneration::one(),
                    registration,
                    profile,
                    registration.tool_names().cloned(),
                    RunnerApprovalPolicy {
                        sandbox: after.sandbox,
                        permission_overrides: &after.permission_overrides,
                    },
                    CredentialProfileGrantState::Active,
                )
            })
            .transpose()?,
        Some(prior) => {
            let revision = prior
                .revision
                .checked_next()
                .ok_or(RunnerDomainError::GenerationExhausted)?;
            match after.credential_profile.clone() {
                Some(profile) => Some(build_grant(
                    session,
                    revision,
                    registration,
                    profile,
                    registration.tool_names().cloned(),
                    RunnerApprovalPolicy {
                        sandbox: after.sandbox,
                        permission_overrides: &after.permission_overrides,
                    },
                    CredentialProfileGrantState::Active,
                )?),
                None => Some(CredentialProfileGrant {
                    revision,
                    state: CredentialProfileGrantState::Revoked,
                    ..prior
                }),
            }
        }
    };
    let after_facts = grant
        .as_ref()
        .map(CredentialProfileGrant::reconstitution_facts);
    let grant_change =
        (before_facts.is_some() || after_facts.is_some()).then_some(RunnerCredentialGrantChange {
            before: before_facts,
            after: after_facts,
        });
    Ok((grant, grant_change))
}

fn resolve_runner_approval(
    effect: RunnerToolEffectClass,
    sandbox: RunnerSandboxProfile,
    permission_overrides: &RunnerToolPermissionOverrides,
    tool: &ToolName,
) -> CredentialToolApproval {
    match permission_overrides.get(tool) {
        Some(RunnerToolPermissionOverride::Auto) => CredentialToolApproval::Automatic,
        Some(RunnerToolPermissionOverride::Confirm) => CredentialToolApproval::SessionPolicy,
        None if sandbox == RunnerSandboxProfile::WorkspaceRestricted => {
            CredentialToolApproval::Automatic
        }
        None if effect == RunnerToolEffectClass::Pure => CredentialToolApproval::Automatic,
        None => CredentialToolApproval::SessionPolicy,
    }
}

struct RunnerApprovalPolicy<'a> {
    sandbox: RunnerSandboxProfile,
    permission_overrides: &'a RunnerToolPermissionOverrides,
}

fn build_grant(
    session: SessionId,
    revision: RunnerGeneration,
    registration: &ValidatedRunnerRegistration,
    profile: CredentialProfileName,
    tools: impl IntoIterator<Item = ToolName>,
    policy: RunnerApprovalPolicy<'_>,
    state: CredentialProfileGrantState,
) -> Result<CredentialProfileGrant, RunnerDomainError> {
    registration
        .profile(&profile)
        .ok_or(RunnerDomainError::CredentialProfileUnavailable)?;
    let tools: BTreeSet<_> = tools.into_iter().collect();
    if tools.iter().any(|tool| registration.tool(tool).is_none()) {
        return Err(RunnerDomainError::ToolUnavailable);
    }
    let approvals = tools
        .iter()
        .map(|tool| {
            let declaration = &registration.tools[tool];
            (
                tool.clone(),
                resolve_runner_approval(
                    declaration.effect,
                    policy.sandbox,
                    policy.permission_overrides,
                    tool,
                ),
            )
        })
        .collect();
    Ok(CredentialProfileGrant {
        session,
        runner: registration.runner,
        revision,
        profile,
        tools,
        approvals,
        state,
    })
}

/// Exact future-dispatch authority resolved from a tool/profile pair.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialDispatchAuthorization {
    /// The owning session identity.
    pub session: SessionId,
    /// The runner authorized to execute the dispatch.
    pub runner: RunnerId,
    /// The credential grant revision authorizing dispatch.
    pub grant_revision: RunnerGeneration,
    /// The runner-local credential profile name.
    pub profile: CredentialProfileName,
    /// The exact tool name.
    pub tool: ToolName,
    /// The approval posture applied to the tool dispatch.
    pub approval: CredentialToolApproval,
}

/// Successful forward-only credential grant replacement.
#[derive(Debug, Eq, PartialEq)]
pub struct CredentialProfileGrantReplacement {
    /// The resulting credential grant, when the selection requires one.
    pub grant: CredentialProfileGrant,
    /// The complete before-and-after change facts.
    pub change: CredentialProfileChange,
}

/// Complete before-and-after profile and tool facts for grant replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialProfileChange {
    /// The owning session identity.
    pub session: SessionId,
    /// The placement or grant revision before replacement.
    pub prior_revision: RunnerGeneration,
    /// The placement or grant revision after replacement.
    pub replacement_revision: RunnerGeneration,
    /// The credential profile before replacement.
    pub before_profile: CredentialProfileName,
    /// The credential profile after replacement.
    pub after_profile: CredentialProfileName,
    /// The granted tool set before replacement.
    pub before_tools: BTreeSet<ToolName>,
    /// The granted tool set after replacement.
    pub after_tools: BTreeSet<ToolName>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ApprovedToolRequest, DangerousToolAutoApproval, DecideToolRequest, DurableCommandId,
        ReconstitutedToolAttempt, ResolvedContextFrontierSnapshot, ToolApprovalDecision,
        ToolApprovalPosture, ToolApprovalResolutionReconstitutionInput, ToolAttemptEnd,
        ToolBatchPhaseReconstitutionInput, ToolBatchReconstitutionInput, ToolExecutionErrorKind,
        ToolRequest, ToolRequestId, ToolRequestOrdinal, ToolRequestReconstitutionInput,
        test_support::{
            context_frontier_id, model_call_id, runner_authentication_id, runner_enrollment_id,
            runner_id, runner_lease_id, session_id, tool_attempt_id, tool_request_id,
            turn_attempt_id, turn_id,
        },
    };

    const ENROLLMENT: u128 = 0x7100;
    const RUNNER: u128 = 0x7200;
    const REPLACEMENT_RUNNER: u128 = 0x7201;
    const THIRD_RUNNER: u128 = 0x7202;
    const AUTHENTICATION: u128 = 0x7300;
    const LEASE: u128 = 0x7400;
    const ATTEMPT: u128 = 0x7500;
    const RETRY_ATTEMPT: u128 = 0x7501;
    const SESSION: u128 = 0x7600;
    /// Arbitrary empty context-frontier identity for complete batch fixtures.
    const YIELDED_FRONTIER: u128 = 0x7a00;

    fn class() -> RunnerCapabilityClass {
        RunnerCapabilityClass::try_new("linux.workspace".to_owned())
            .expect("the canonical class name is valid")
    }

    fn profile(name: &str) -> CredentialProfileName {
        CredentialProfileName::try_new(name.to_owned()).expect("fixture profile names are valid")
    }

    fn tool(name: &str) -> ToolName {
        ToolName::try_new(name.to_owned()).expect("fixture tool names are valid")
    }

    fn repository_key() -> WorkspaceRepositoryKey {
        WorkspaceRepositoryKey::try_new("signalbox".to_owned())
            .expect("the fixture repository key is valid")
    }

    fn model_definition(name: &str) -> RunnerToolModelDefinition {
        RunnerToolModelDefinition::try_new(
            format!("Run the {name} fixture operation"),
            r#"{"type":"object"}"#.to_owned(),
        )
        .expect("fixture model definitions are valid")
    }

    fn sandbox_profiles() -> [RunnerSandboxProfile; 2] {
        [
            RunnerSandboxProfile::Ambient,
            RunnerSandboxProfile::WorkspaceRestricted,
        ]
    }

    fn no_permission_overrides() -> RunnerToolPermissionOverrides {
        RunnerToolPermissionOverrides::try_new([])
            .expect("the empty permission override fixture is valid")
    }

    fn catalog() -> RunnerCatalog {
        let inspect = RunnerToolDeclaration::new(
            tool("inspect"),
            model_definition("inspect"),
            ToolPermissionDefault::Auto,
            RunnerToolEffectClass::Pure,
            ToolAdmissibleLoci::DaemonOrRunner {
                selector: RunnerSelector::CapabilityClass(class()),
            },
        );
        let deploy = RunnerToolDeclaration::new(
            tool("deploy"),
            model_definition("deploy"),
            ToolPermissionDefault::Confirm,
            RunnerToolEffectClass::SideEffecting,
            ToolAdmissibleLoci::RunnerOnly {
                selector: RunnerSelector::CapabilityClass(class()),
            },
        );
        let sync = RunnerToolDeclaration::new(
            tool("sync"),
            model_definition("sync"),
            ToolPermissionDefault::Confirm,
            RunnerToolEffectClass::Idempotent,
            ToolAdmissibleLoci::RunnerOnly {
                selector: RunnerSelector::CapabilityClass(class()),
            },
        );
        let readonly = CredentialProfilePolicy::try_new(
            profile("readonly"),
            [
                (tool("inspect"), CredentialToolApproval::Automatic),
                (tool("sync"), CredentialToolApproval::SessionPolicy),
            ],
        )
        .expect("the profile references a declared fixture tool");
        let admin = CredentialProfilePolicy::try_new(
            profile("admin"),
            [(tool("deploy"), CredentialToolApproval::SessionPolicy)],
        )
        .expect("the profile references a declared fixture tool");
        RunnerCatalog::try_new(
            [class()],
            [inspect, deploy, sync],
            [readonly, admin],
            [WorkspaceCapability::WorktreePerSession],
            sandbox_profiles(),
        )
        .expect("the canonical catalog is internally consistent")
    }

    fn enrollment_for(runner: RunnerId) -> RunnerEnrollment {
        RunnerEnrollment::new(
            runner_enrollment_id(ENROLLMENT),
            runner,
            runner_authentication_id(AUTHENTICATION),
            [class()],
        )
    }

    fn enrollment() -> RunnerEnrollment {
        enrollment_for(runner_id(RUNNER))
    }

    fn enrollment_for_registration(registration: &ValidatedRunnerRegistration) -> RunnerEnrollment {
        RunnerEnrollment {
            enrollment: registration.enrollment,
            runner: registration.runner,
            authentication: registration.authentication,
            allowed_classes: BTreeSet::from([class()]),
            state: RunnerEnrollmentState::Active,
            registration_revision: Arc::clone(&registration.current_revision),
            registration_active: Arc::clone(&registration.enrollment_active),
            registration_preparation: Arc::new(AtomicBool::new(false)),
        }
    }

    fn advertisement() -> RunnerAdvertisement {
        RunnerAdvertisement::new(
            [class()],
            [tool("inspect"), tool("deploy"), tool("sync")],
            [profile("readonly"), profile("admin")],
            [WorkspaceCapability::WorktreePerSession],
            sandbox_profiles(),
            [RunnerRepositoryEntry::new(repository_key(), None)],
        )
    }

    fn registration_for(runner: RunnerId) -> ValidatedRunnerRegistration {
        enrollment_for(runner)
            .register(advertisement(), &catalog())
            .expect("the advertisement is a subset of daemon policy")
    }

    fn registration() -> ValidatedRunnerRegistration {
        registration_for(runner_id(RUNNER))
    }

    fn placement_request(profile: CredentialProfileName) -> SessionRunnerPlacementRequest {
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: Some(profile),
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
        }
    }

    fn profileless_placement_request() -> SessionRunnerPlacementRequest {
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: None,
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
        }
    }

    fn exact_placement_request(runner: RunnerId) -> SessionRunnerPlacementRequest {
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::Identity(runner),
            working_directory: WorkingDirectorySelection::Exact(directory("/workspace/session")),
            credential_profile: None,
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            permission_overrides: no_permission_overrides(),
        }
    }

    fn directory(value: &str) -> RunnerWorkingDirectory {
        RunnerWorkingDirectory::try_new(value.to_owned())
            .expect("fixture working directories are valid")
    }

    fn lease_offer_request(tool_name: &str) -> RunnerLeaseOfferRequest {
        RunnerLeaseOfferRequest {
            lease: runner_lease_id(LEASE),
            tool: tool(tool_name),
        }
    }

    fn request(tool_name: &str) -> ToolRequest {
        let request_seed = match tool_name {
            "inspect" => 0x7700,
            "sync" => 0x7701,
            "deploy" => 0x7702,
            _ => panic!("the fixture tool must be declared"),
        };
        ToolRequestReconstitutionInput::new(
            tool_request_id(request_seed),
            session_id(SESSION),
            turn_id(0x7800),
            model_call_id(0x7900),
            ToolRequestOrdinal::from_u32(0),
            tool(tool_name),
            NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("fixture arguments are canonical"),
        )
        .into_request()
    }

    fn approved_request(tool_name: &str) -> ApprovedToolRequest {
        let request = request(tool_name);
        let request_seed = request.id().as_uuid().as_u128();
        let command = DecideToolRequest::new(
            DurableCommandId::from_uuid(uuid::Uuid::from_u128(request_seed + 0x300)),
            request.id(),
            ToolApprovalDecision::Approve,
        );
        let prepared = command
            .prepare_applied(&request)
            .expect("the fixture request and decision correlate");
        let crate::DecideToolRequestResult::Applied(applied) = prepared.result() else {
            panic!("the approving fixture decision applies")
        };
        ApprovedToolRequest::try_from_resolution(request, applied.resolution().clone())
            .expect("the fixture approval matches its request")
    }

    fn claimed_batch(tool_name: &str, effect: RunnerToolEffectClass) -> ToolBatch {
        claimed_batch_with_issuing_attempt(tool_name, effect, turn_attempt_id(0x7b00))
    }

    fn claimed_batch_with_issuing_attempt(
        tool_name: &str,
        effect: RunnerToolEffectClass,
        issuing_attempt: crate::TurnAttemptId,
    ) -> ToolBatch {
        let approved = approved_request(tool_name);
        let current = approved
            .prepare_attempt(
                tool_attempt_id(ATTEMPT),
                issuing_attempt,
                tool_effect_class(effect),
            )
            .authorize()
            .expect("the claimed fixture attempt authorizes once")
            .into_parts()
            .0;
        ToolBatchReconstitutionInput::new(
            session_id(SESSION),
            turn_id(0x7800),
            model_call_id(0x7900),
            ResolvedContextFrontierSnapshot::try_from_candidate(
                session_id(SESSION),
                context_frontier_id(YIELDED_FRONTIER),
                Vec::new(),
            )
            .expect("an empty fixture snapshot is valid"),
            vec![approved.request().clone()],
            vec![approved.approval().clone()],
            vec![ReconstitutedToolAttempt::Current(current)],
            ToolBatchPhaseReconstitutionInput::Executing {
                turn_attempt: issuing_attempt,
            },
        )
        .reconstitute()
        .expect("the claimed fixture batch is complete")
    }

    fn current_attempt_id(batch: &ToolBatch, request: ToolRequestId) -> Option<ToolAttemptId> {
        batch.attempt(request).map(|attempt| match attempt {
            ReconstitutedToolAttempt::Current(current) => current.attempt(),
            ReconstitutedToolAttempt::Ended(ended) => ended.attempt(),
        })
    }

    fn current_attempt_effect_class(
        batch: &ToolBatch,
        request: ToolRequestId,
    ) -> Option<ToolEffectClass> {
        batch.attempt(request).map(|attempt| match attempt {
            ReconstitutedToolAttempt::Current(current) => current.effect_class(),
            ReconstitutedToolAttempt::Ended(ended) => ended.effect_class(),
        })
    }

    fn placement_grant_lineage(
        placement: &SessionRunnerPlacement,
    ) -> Option<RunnerCredentialGrantLineage> {
        match placement.state() {
            SessionRunnerPlacementState::Pinned(pinned) => pinned.grant_lineage,
            SessionRunnerPlacementState::RunnerLost(lost) => lost.pinned.grant_lineage,
            SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::Pinned(
                lost,
            )) => lost.pinned.grant_lineage,
            SessionRunnerPlacementState::Unpinned
            | SessionRunnerPlacementState::RunnerLostBeforePin(_)
            | SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::BeforePin(
                _,
            )) => None,
        }
    }

    fn automatically_approved_request(tool_name: &str) -> ApprovedToolRequest {
        let request = request(tool_name);
        let approval = ToolApprovalResolutionReconstitutionInput::policy_auto(request.id())
            .reconstitute()
            .expect("the fixture registry policy approves");
        ApprovedToolRequest::try_from_resolution(request, approval)
            .expect("the fixture approval matches its request")
    }

    fn blanket_approved_request(tool_name: &str) -> ApprovedToolRequest {
        let request = request(tool_name);
        let approval = ToolApprovalResolutionReconstitutionInput::session_blanket(
            request.id(),
            DangerousToolAutoApproval::ApproveAll,
        )
        .reconstitute()
        .expect("the fixture session blanket approves");
        ApprovedToolRequest::try_from_resolution(request, approval)
            .expect("the fixture approval matches its request")
    }

    fn authorized(
        tool_name: &str,
        attempt: ToolAttemptId,
        effect: RunnerToolEffectClass,
    ) -> RunnerToolAttemptAuthorization {
        let effect = match effect {
            RunnerToolEffectClass::Pure => ToolEffectClass::EffectFree,
            RunnerToolEffectClass::Idempotent | RunnerToolEffectClass::SideEffecting => {
                ToolEffectClass::ExternalEffect
            }
        };
        let approved = approved_request(tool_name);
        let authorized = approved
            .prepare_attempt(attempt, turn_attempt_id(0x7b00), effect)
            .authorize()
            .expect("the prepared fixture attempt authorizes once");
        RunnerToolAttemptAuthorization::try_new(approved, authorized)
            .expect("the approved request binds the authorized attempt")
    }

    fn automatically_authorized(
        tool_name: &str,
        attempt: ToolAttemptId,
        effect: RunnerToolEffectClass,
    ) -> RunnerToolAttemptAuthorization {
        let approved = automatically_approved_request(tool_name);
        let authorized = approved
            .prepare_attempt(attempt, turn_attempt_id(0x7b00), tool_effect_class(effect))
            .authorize()
            .expect("the prepared fixture attempt authorizes once");
        RunnerToolAttemptAuthorization::try_new(approved, authorized)
            .expect("the approved request binds the authorized attempt")
    }

    fn blanket_authorized(
        tool_name: &str,
        attempt: ToolAttemptId,
        effect: RunnerToolEffectClass,
    ) -> RunnerToolAttemptAuthorization {
        let approved = blanket_approved_request(tool_name);
        let authorized = approved
            .prepare_attempt(attempt, turn_attempt_id(0x7b00), tool_effect_class(effect))
            .authorize()
            .expect("the prepared fixture attempt authorizes once");
        RunnerToolAttemptAuthorization::try_new(approved, authorized)
            .expect("the approved request binds the authorized attempt")
    }

    fn user_override_approved_request(tool_name: &str) -> ApprovedToolRequest {
        const OVERRIDE_COMMAND: u128 = 0x7c00;
        const DENIED_REQUEST: u128 = 0x7c01;

        let request = request(tool_name);
        let approval = ToolApprovalResolutionReconstitutionInput::user_override(
            request.id(),
            DurableCommandId::from_uuid(uuid::Uuid::from_u128(OVERRIDE_COMMAND)),
            tool_request_id(DENIED_REQUEST),
            ToolApprovalPosture::Delegated,
        )
        .reconstitute()
        .expect("the fixture override consumes under the frozen delegated posture");
        ApprovedToolRequest::try_from_resolution(request, approval)
            .expect("the fixture approval matches its request")
    }

    fn user_override_authorized(
        tool_name: &str,
        attempt: ToolAttemptId,
        effect: RunnerToolEffectClass,
    ) -> RunnerToolAttemptAuthorization {
        let approved = user_override_approved_request(tool_name);
        let authorized = approved
            .prepare_attempt(attempt, turn_attempt_id(0x7b00), tool_effect_class(effect))
            .authorize()
            .expect("the prepared fixture attempt authorizes once");
        RunnerToolAttemptAuthorization::try_new(approved, authorized)
            .expect("the approved request binds the authorized attempt")
    }

    fn declared_effect(tool_name: &str) -> RunnerToolEffectClass {
        match tool_name {
            "inspect" => RunnerToolEffectClass::Pure,
            "sync" => RunnerToolEffectClass::Idempotent,
            "deploy" => RunnerToolEffectClass::SideEffecting,
            _ => panic!("the fixture tool must have a declared effect"),
        }
    }

    fn pinned(profile_name: &str) -> (ValidatedRunnerRegistration, SessionRunnerPin) {
        let registration = registration();
        let pin = SessionRunnerPlacement::new(
            session_id(SESSION),
            placement_request(profile(profile_name)),
        )
        .pin_and_offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            directory("/workspace/session"),
            None,
            authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        )
        .expect("the registration and authorized attempt satisfy placement");
        (registration, pin)
    }

    fn pinned_with_confirm_override(
        profile_name: &str,
    ) -> (ValidatedRunnerRegistration, SessionRunnerPin) {
        let registration = registration();
        let mut request = placement_request(profile(profile_name));
        request.permission_overrides = RunnerToolPermissionOverrides::try_new([(
            tool("inspect"),
            RunnerToolPermissionOverride::Confirm,
        )])
        .expect("the exact confirmation override is valid");
        let pin = SessionRunnerPlacement::new(session_id(SESSION), request)
            .pin_and_offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                directory("/workspace/session"),
                None,
                authorized(
                    "inspect",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            )
            .expect("the confirmation override accepts an exact user decision");
        (registration, pin)
    }

    fn omit_runner_required_tool(placement: &mut SessionRunnerPlacement, omitted: &ToolName) {
        let SessionRunnerPlacementState::Pinned(stored) = &mut placement.state else {
            panic!("the fixture placement is pinned")
        };
        stored.runner_required_tools.remove(omitted);
    }

    fn offered(
        tool_name: &str,
        attempt: ToolAttemptId,
    ) -> (
        ValidatedRunnerRegistration,
        SessionRunnerPlacement,
        Option<CredentialProfileGrant>,
        RunnerLease,
    ) {
        let registration = registration();
        let pin = SessionRunnerPlacement::new(
            session_id(SESSION),
            placement_request(profile("readonly")),
        )
        .pin_and_offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            directory("/workspace/session"),
            None,
            authorized(tool_name, attempt, declared_effect(tool_name)),
            lease_offer_request(tool_name),
        )
        .expect("the first authorized lease pins the fixture placement");
        (registration, pin.placement, pin.grant, pin.lease)
    }

    fn offered_from_batch(
        tool_name: &str,
    ) -> (
        ValidatedRunnerRegistration,
        SessionRunnerPlacement,
        Option<CredentialProfileGrant>,
        ToolBatch,
        RunnerLease,
    ) {
        let registration = registration();
        let batch = claimed_batch(tool_name, declared_effect(tool_name));
        let authorization = batch
            .resume_runner_attempt(tool_attempt_id(ATTEMPT))
            .expect("the owning batch issues the fixture runner authority once");
        let pin = SessionRunnerPlacement::new(
            session_id(SESSION),
            placement_request(profile("readonly")),
        )
        .pin_and_offer_lease(
            &enrollment_for_registration(&registration),
            &registration,
            directory("/workspace/session"),
            None,
            authorization,
            lease_offer_request(tool_name),
        )
        .expect("batch-issued authority pins the fixture placement");
        (registration, pin.placement, pin.grant, batch, pin.lease)
    }

    fn placement_without_required_tool(
        mut placement: SessionRunnerPlacement,
        missing: &ToolName,
    ) -> SessionRunnerPlacement {
        let SessionRunnerPlacementState::Pinned(pinned) = &mut placement.state else {
            panic!("the fixture placement must be pinned")
        };
        pinned.runner_required_tools.remove(missing);
        placement
    }

    fn placement_without_grant_lineage(
        mut placement: SessionRunnerPlacement,
    ) -> SessionRunnerPlacement {
        let SessionRunnerPlacementState::Pinned(pinned) = &mut placement.state else {
            panic!("the fixture placement must be pinned")
        };
        pinned.grant_lineage = None;
        placement
    }

    fn placement_with_grant_lineage(
        mut placement: SessionRunnerPlacement,
        lineage: RunnerCredentialGrantLineage,
    ) -> SessionRunnerPlacement {
        let SessionRunnerPlacementState::Pinned(pinned) = &mut placement.state else {
            panic!("the fixture placement must be pinned")
        };
        pinned.grant_lineage = Some(lineage);
        placement
    }

    fn enrollment_reconstitution_input() -> RunnerEnrollmentReconstitutionInput {
        RunnerEnrollmentReconstitutionInput {
            enrollment: runner_enrollment_id(ENROLLMENT),
            recorded_enrollment: runner_enrollment_id(ENROLLMENT),
            runner: runner_id(RUNNER),
            recorded_runner: runner_id(RUNNER),
            authentication: runner_authentication_id(AUTHENTICATION),
            recorded_authentication: runner_authentication_id(AUTHENTICATION),
            allowed_classes: BTreeSet::from([class()]),
            recorded_allowed_classes: BTreeSet::from([class()]),
            registration_revision: None,
            recorded_registration_revision: None,
            state: RunnerEnrollmentState::Active,
            recorded_state: RunnerEnrollmentState::Active,
        }
    }

    fn lease_reconstitution_input(lease: RunnerLease) -> RunnerLeaseReconstitutionInput {
        RunnerLeaseReconstitutionInput {
            lease: lease.lease,
            dispatch: lease.dispatch,
            runner: lease.runner,
            tool: lease.tool.clone(),
            effect: lease.effect,
            credential_authorization: lease.credential_authorization.clone(),
            generation: lease.generation,
            state: lease.state,
            recorded_correlation: lease.correlation(),
            recorded_session: lease.dispatch.session(),
            recorded_effect: lease.effect,
            recorded_credential_authorization: lease.credential_authorization.clone(),
            recorded_state: lease.state,
            retry_preparation: RunnerLeaseRetryPreparation::Available,
        }
    }

    fn borrowed_lease_reconstitution_input(lease: &RunnerLease) -> RunnerLeaseReconstitutionInput {
        RunnerLeaseReconstitutionInput {
            lease: lease.lease,
            dispatch: lease.dispatch,
            runner: lease.runner,
            tool: lease.tool.clone(),
            effect: lease.effect,
            credential_authorization: lease.credential_authorization.clone(),
            generation: lease.generation,
            state: lease.state,
            recorded_correlation: lease.correlation(),
            recorded_session: lease.dispatch.session(),
            recorded_effect: lease.effect,
            recorded_credential_authorization: lease.credential_authorization.clone(),
            recorded_state: lease.state,
            retry_preparation: RunnerLeaseRetryPreparation::Available,
        }
    }

    fn no_execution_proof(lease: &RunnerLease) -> RunnerLeaseNoExecutionProof {
        RunnerLeaseNoExecutionProof {
            correlation: lease.correlation(),
        }
    }

    fn placement_reconstitution_input(
        placement: SessionRunnerPlacement,
    ) -> SessionRunnerPlacementReconstitutionInput {
        SessionRunnerPlacementReconstitutionInput {
            session: placement.session,
            revision: placement.revision,
            request: placement.request,
            state: placement.state,
            history: RunnerPlacementReconstitutionHistory::Initial,
        }
    }

    fn grant_reconstitution_input(
        grant: CredentialProfileGrant,
    ) -> CredentialProfileGrantReconstitutionInput {
        CredentialProfileGrantReconstitutionInput {
            session: grant.session,
            runner: grant.runner,
            revision: grant.revision,
            profile: grant.profile,
            tools: grant.tools,
            approvals: grant.approvals,
            state: grant.state,
        }
    }

    #[test]
    fn s30_runner_catalog_names_are_portable_and_bounded() {
        assert_eq!(
            RunnerCapabilityClass::try_new("-leading".to_owned()),
            Err(RunnerDomainError::InvalidName)
        );
        assert_eq!(
            CredentialProfileName::try_new("contains space".to_owned()),
            Err(RunnerDomainError::InvalidName)
        );
        assert_eq!(
            RunnerCapabilityClass::try_new("x".repeat(NAME_MAX_BYTES + 1)),
            Err(RunnerDomainError::TooLong)
        );
    }

    #[test]
    fn s30_clone_url_digest_rejects_nonhex_text() {
        assert_eq!(
            CanonicalCloneUrlDigest::try_new("g".repeat(64)),
            Err(RunnerDomainError::InvalidHex)
        );
    }

    #[test]
    fn s30_clone_url_digest_rejects_wrong_length() {
        assert_eq!(
            CanonicalCloneUrlDigest::try_new("a".repeat(65)),
            Err(RunnerDomainError::InvalidHex)
        );
    }

    #[test]
    fn s30_workspace_revision_rejects_abbreviated_or_overlong_object_ids() {
        assert_eq!(
            WorkspaceRevision::try_new("a".repeat(39)),
            Err(RunnerDomainError::InvalidHex)
        );
        assert_eq!(
            WorkspaceRevision::try_new("a".repeat(41)),
            Err(RunnerDomainError::InvalidHex)
        );
    }

    #[test]
    fn s30_workspace_revision_rejects_uppercase_object_id() {
        assert_eq!(
            WorkspaceRevision::try_new("A".repeat(40)),
            Err(RunnerDomainError::InvalidHex)
        );
    }

    #[test]
    fn s30_workspace_branch_rejects_dot_dot() {
        assert_eq!(
            WorkspaceBranchName::try_new("bad..branch".to_owned()),
            Err(RunnerDomainError::InvalidBranchName)
        );
    }

    #[test]
    fn s30_workspace_branch_rejects_lock_suffix() {
        assert_eq!(
            WorkspaceBranchName::try_new("component.lock".to_owned()),
            Err(RunnerDomainError::InvalidBranchName)
        );
    }

    #[test]
    fn s30_workspace_branch_rejects_reflog_syntax() {
        assert_eq!(
            WorkspaceBranchName::try_new("bad@{branch".to_owned()),
            Err(RunnerDomainError::InvalidBranchName)
        );
    }

    #[test]
    fn s30_workspace_branch_rejects_single_at() {
        assert!(matches!(
            WorkspaceBranchName::try_new("@".to_owned()),
            Err(RunnerDomainError::InvalidBranchName)
        ));
    }

    #[test]
    fn s30_workspace_branch_accepts_closing_bracket() {
        assert!(WorkspaceBranchName::try_new("topic]ok".to_owned()).is_ok());
    }

    #[test]
    fn s30_workspace_relative_path_rejects_absolute_value() {
        assert_eq!(
            WorkspaceRelativePath::try_new("/sessions/one".to_owned()),
            Err(RunnerDomainError::InvalidRelativePath)
        );
    }

    #[test]
    fn s30_workspace_relative_path_rejects_parent_traversal() {
        assert_eq!(
            WorkspaceRelativePath::try_new("sessions/../one".to_owned()),
            Err(RunnerDomainError::InvalidRelativePath)
        );
    }

    #[test]
    fn s30_workspace_relative_path_rejects_empty_component() {
        assert_eq!(
            WorkspaceRelativePath::try_new("sessions//one".to_owned()),
            Err(RunnerDomainError::InvalidRelativePath)
        );
    }

    #[test]
    fn s30_permission_overrides_reject_duplicate_tools() {
        assert_eq!(
            RunnerToolPermissionOverrides::try_new([
                (tool("inspect"), RunnerToolPermissionOverride::Auto),
                (tool("inspect"), RunnerToolPermissionOverride::Confirm),
            ]),
            Err(RunnerDomainError::DuplicateTool(tool("inspect")))
        );
    }

    #[test]
    fn s30_permission_overrides_reject_more_than_sixty_four_tools() {
        assert_eq!(
            RunnerToolPermissionOverrides::try_new((0..=PERMISSION_OVERRIDE_MAX_ENTRIES).map(
                |index| (
                    tool(&format!("tool_{index}")),
                    RunnerToolPermissionOverride::Auto,
                )
            ),),
            Err(RunnerDomainError::TooManyPermissionOverrides)
        );
    }

    #[test]
    fn s30_runner_tool_model_definition_requires_a_json_object_schema() {
        assert_eq!(
            RunnerToolModelDefinition::try_new("Inspect the workspace".to_owned(), "[]".to_owned()),
            Err(RunnerDomainError::InvalidToolInputSchema)
        );
    }

    #[test]
    fn s30_workspace_repository_keys_use_the_catalog_name_contract() {
        let accepted = WorkspaceRepositoryKey::try_new("r".repeat(NAME_MAX_BYTES))
            .expect("the catalog-name maximum is accepted");

        assert_eq!(accepted.as_str().len(), NAME_MAX_BYTES);
        assert_eq!(
            WorkspaceRepositoryKey::try_new("r".repeat(NAME_MAX_BYTES + 1)),
            Err(RunnerDomainError::TooLong)
        );
        assert_eq!(
            WorkspaceRepositoryKey::try_new("contains space".to_owned()),
            Err(RunnerDomainError::InvalidName)
        );
    }

    #[test]
    fn s30_catalog_rejects_duplicate_capability_class() {
        assert_eq!(
            RunnerCatalog::try_new([class(), class()], [], [], [], []),
            Err(RunnerDomainError::DuplicateCapabilityClass(class()))
        );
    }

    #[test]
    fn s30_catalog_rejects_duplicate_sandbox_profile() {
        assert_eq!(
            RunnerCatalog::try_new(
                [],
                [],
                [],
                [],
                [RunnerSandboxProfile::Ambient, RunnerSandboxProfile::Ambient],
            ),
            Err(RunnerDomainError::DuplicateSandboxProfile(
                RunnerSandboxProfile::Ambient
            ))
        );
    }

    #[test]
    fn s30_catalog_rejects_duplicate_workspace_capability() {
        assert_eq!(
            RunnerCatalog::try_new(
                [],
                [],
                [],
                [
                    WorkspaceCapability::WorktreePerSession,
                    WorkspaceCapability::WorktreePerSession,
                ],
                [],
            ),
            Err(RunnerDomainError::DuplicateWorkspaceCapability(
                WorkspaceCapability::WorktreePerSession
            ))
        );
    }

    #[test]
    fn s30_logical_enrollment_retains_distinct_typed_identities() {
        let enrollment = RunnerEnrollment::new(
            runner_enrollment_id(ENROLLMENT),
            runner_id(RUNNER),
            runner_authentication_id(AUTHENTICATION),
            [class()],
        );

        assert_eq!(enrollment.enrollment(), runner_enrollment_id(ENROLLMENT));
        assert_eq!(enrollment.runner(), runner_id(RUNNER));
        assert_eq!(
            enrollment.authentication(),
            runner_authentication_id(AUTHENTICATION)
        );
        let (_, _, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT));
        assert_eq!(lease.correlation().lease, runner_lease_id(LEASE));
    }

    #[test]
    fn s30_unknown_advertised_tool_rejects_the_complete_registration() {
        let enrollment = RunnerEnrollment::new(
            runner_enrollment_id(ENROLLMENT),
            runner_id(RUNNER),
            runner_authentication_id(AUTHENTICATION),
            [class()],
        );
        let advertisement = RunnerAdvertisement::new([class()], [tool("unknown")], [], [], [], []);

        assert_eq!(
            enrollment.register(advertisement, &catalog()),
            Err(RunnerDomainError::ToolUndeclared(tool("unknown")))
        );
    }

    #[test]
    fn s30_catalog_rejects_tool_selector_for_undeclared_class() {
        let declaration = RunnerToolDeclaration::new(
            tool("specialized"),
            model_definition("specialized"),
            ToolPermissionDefault::Auto,
            RunnerToolEffectClass::Pure,
            ToolAdmissibleLoci::RunnerOnly {
                selector: RunnerSelector::CapabilityClass(class()),
            },
        );

        assert_eq!(
            RunnerCatalog::try_new([], [declaration], [], [], []),
            Err(RunnerDomainError::CapabilityClassNotAllowed(class()))
        );
    }

    #[test]
    fn s30_catalog_rejects_idempotent_tool_with_daemon_locus() {
        let declaration = RunnerToolDeclaration::new(
            tool("sync"),
            model_definition("sync"),
            ToolPermissionDefault::Confirm,
            RunnerToolEffectClass::Idempotent,
            ToolAdmissibleLoci::DaemonOrRunner {
                selector: RunnerSelector::CapabilityClass(class()),
            },
        );

        assert_eq!(
            RunnerCatalog::try_new([class()], [declaration], [], [], []),
            Err(RunnerDomainError::UnsupportedDaemonIdempotency(tool(
                "sync"
            )))
        );
    }

    #[test]
    fn s30_advertised_class_requires_enrollment_and_catalog_authority() {
        let enrollment = RunnerEnrollment::new(
            runner_enrollment_id(ENROLLMENT),
            runner_id(RUNNER),
            runner_authentication_id(AUTHENTICATION),
            [class()],
        );
        let catalog = RunnerCatalog::try_new([], [], [], [], [])
            .expect("the empty catalog is internally consistent");
        let advertisement = RunnerAdvertisement::new([class()], [], [], [], [], []);

        assert_eq!(
            enrollment.register(advertisement, &catalog),
            Err(RunnerDomainError::CapabilityClassNotAllowed(class()))
        );
    }

    #[test]
    fn s30_daemon_only_tool_rejects_the_complete_registration() {
        let enrollment = RunnerEnrollment::new(
            runner_enrollment_id(ENROLLMENT),
            runner_id(RUNNER),
            runner_authentication_id(AUTHENTICATION),
            [class()],
        );
        let daemon_only = RunnerToolDeclaration::new(
            tool("daemon"),
            model_definition("daemon"),
            ToolPermissionDefault::Auto,
            RunnerToolEffectClass::Pure,
            ToolAdmissibleLoci::DaemonOnly,
        );
        let catalog = RunnerCatalog::try_new([], [daemon_only], [], [], [])
            .expect("the daemon-only declaration is internally consistent");
        let advertisement = RunnerAdvertisement::new([], [tool("daemon")], [], [], [], []);

        assert_eq!(
            enrollment.register(advertisement, &catalog),
            Err(RunnerDomainError::ToolLocusNotAllowed(tool("daemon")))
        );
    }

    #[test]
    fn s30_tool_selector_must_match_advertised_runner_capability() {
        let enrollment = RunnerEnrollment::new(
            runner_enrollment_id(ENROLLMENT),
            runner_id(RUNNER),
            runner_authentication_id(AUTHENTICATION),
            [class()],
        );
        let declaration = RunnerToolDeclaration::new(
            tool("specialized"),
            model_definition("specialized"),
            ToolPermissionDefault::Auto,
            RunnerToolEffectClass::Pure,
            ToolAdmissibleLoci::RunnerOnly {
                selector: RunnerSelector::Identity(runner_id(REPLACEMENT_RUNNER)),
            },
        );
        let catalog = RunnerCatalog::try_new([], [declaration], [], [], [])
            .expect("the identity-targeted declaration is internally consistent");
        let advertisement = RunnerAdvertisement::new([], [tool("specialized")], [], [], [], []);

        assert_eq!(
            enrollment.register(advertisement, &catalog),
            Err(RunnerDomainError::ToolLocusNotAllowed(tool("specialized")))
        );
    }

    #[test]
    fn s30_registration_rejects_unadvertised_sandbox_profile() {
        let advertisement =
            RunnerAdvertisement::new([class()], [], [], [], [RunnerSandboxProfile::Ambient], []);
        let restricted_catalog = RunnerCatalog::try_new(
            [class()],
            [],
            [],
            [],
            [RunnerSandboxProfile::WorkspaceRestricted],
        )
        .expect("the restricted catalog is internally consistent");

        assert_eq!(
            enrollment().register(advertisement, &restricted_catalog),
            Err(RunnerDomainError::SandboxProfileNotAllowed(
                RunnerSandboxProfile::Ambient
            ))
        );
    }

    #[test]
    fn s30_registration_rejects_repository_profile_outside_advertisement() {
        let advertisement = RunnerAdvertisement::new(
            [class()],
            [],
            [],
            [],
            [],
            [RunnerRepositoryEntry::new(
                repository_key(),
                Some(profile("readonly")),
            )],
        );

        assert_eq!(
            enrollment().register(advertisement, &catalog()),
            Err(RunnerDomainError::RepositoryProfileUnavailable(profile(
                "readonly"
            )))
        );
    }

    #[test]
    fn s30_registration_rejects_oversized_repository_inventory() {
        let repositories = (0..=RunnerAdvertisement::MAX_REPOSITORIES).map(|index| {
            RunnerRepositoryEntry::new(
                WorkspaceRepositoryKey::try_new(format!("repository_{index}"))
                    .expect("the generated repository key is valid"),
                None,
            )
        });
        let advertisement = RunnerAdvertisement::new([class()], [], [], [], [], repositories);

        assert_eq!(
            enrollment().register(advertisement, &catalog()),
            Err(RunnerDomainError::TooManyAdvertisedRepositories)
        );
    }

    #[test]
    fn s30_revoked_enrollment_cannot_register() {
        let enrollment = RunnerEnrollment::new(
            runner_enrollment_id(ENROLLMENT),
            runner_id(RUNNER),
            runner_authentication_id(AUTHENTICATION),
            [class()],
        )
        .revoke()
        .expect("an active enrollment can be revoked");

        assert_eq!(
            enrollment.register(RunnerAdvertisement::new([], [], [], [], [], []), &catalog()),
            Err(RunnerDomainError::EnrollmentRevoked)
        );
    }

    #[test]
    fn s30_outstanding_preparation_excludes_concurrent_registration() {
        let enrollment = enrollment();
        let outstanding = enrollment
            .prepare_registration(advertisement(), &catalog())
            .expect("the pristine enrollment prepares its first registration");

        assert_eq!(
            enrollment.register(advertisement(), &catalog()),
            Err(RunnerDomainError::RegistrationInProgress)
        );
        drop(outstanding);
        let registration = enrollment
            .register(advertisement(), &catalog())
            .expect("an abandoned preparation releases the exclusive fence");
        assert_eq!(registration.revision(), RunnerGeneration::one());
    }

    #[test]
    fn s30_committed_preparation_releases_the_exclusive_fence() {
        let enrollment = enrollment();
        let first = enrollment
            .prepare_registration(advertisement(), &catalog())
            .expect("the pristine enrollment prepares its first registration")
            .commit()
            .expect("the sole outstanding preparation commits");

        let second = enrollment
            .register(advertisement(), &catalog())
            .expect("a committed preparation releases the exclusive fence");
        assert_eq!(Some(second.revision()), first.revision().checked_next());
    }

    #[test]
    fn s30_enrollment_reports_its_last_issued_registration_revision() {
        let enrollment = enrollment();
        assert_eq!(enrollment.last_issued_registration_revision(), None);

        let registration = enrollment
            .register(advertisement(), &catalog())
            .expect("the pristine enrollment issues its first registration");
        assert_eq!(
            enrollment.last_issued_registration_revision(),
            Some(registration.revision())
        );
    }

    #[test]
    fn s31_revoked_enrollment_cannot_authorize_a_later_lease() {
        let (registration, pin) = pinned("readonly");
        let revoked = enrollment()
            .revoke()
            .expect("an active enrollment can be revoked");

        assert_eq!(
            pin.placement.offer_lease(
                &revoked,
                &registration,
                pin.grant.as_ref(),
                authorized(
                    "inspect",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            ),
            Err(RunnerDomainError::EnrollmentRevoked)
        );
    }

    #[test]
    fn s32_revocation_invalidates_registration_for_grant_transition() {
        let enrollment = enrollment();
        let registration = enrollment
            .register(advertisement(), &catalog())
            .expect("the active enrollment issues a registration");
        let mut pin = SessionRunnerPlacement::new(
            session_id(SESSION),
            placement_request(profile("readonly")),
        )
        .pin_and_offer_lease(
            &enrollment,
            &registration,
            directory("/workspace/session"),
            None,
            authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        )
        .expect("the active registration pins its runner");
        let grant = pin.grant.take().expect("profile selection creates a grant");
        let _revoked = enrollment
            .revoke()
            .expect("revocation invalidates retained registration authority");

        assert_eq!(
            pin.placement.replace_credential_profile(
                grant,
                &registration,
                profile("admin"),
                BTreeSet::from([tool("deploy")]),
            ),
            Err(RunnerDomainError::RegistrationChanged)
        );
    }

    #[test]
    fn s31_lease_rejects_a_foreign_active_enrollment() {
        let (registration, pin) = pinned("readonly");
        let foreign = enrollment_for(runner_id(REPLACEMENT_RUNNER));

        assert_eq!(
            pin.placement.offer_lease(
                &foreign,
                &registration,
                pin.grant.as_ref(),
                authorized(
                    "inspect",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            ),
            Err(RunnerDomainError::CorrelationMismatch)
        );
    }

    #[test]
    fn s30_registration_attaches_daemon_policy_not_runner_policy() {
        let registration = registration();
        let declaration = registration
            .tool(&tool("deploy"))
            .expect("the advertised tool is validated");

        assert_eq!(declaration.permission(), ToolPermissionDefault::Confirm);
        assert_eq!(declaration.effect(), RunnerToolEffectClass::SideEffecting);
    }

    #[test]
    fn s30_reregistration_retires_prior_registration_authority() {
        let enrollment = enrollment();
        let initial = enrollment
            .register(advertisement(), &catalog())
            .expect("the initial advertisement is valid");
        let retained = initial.clone();
        let pin = SessionRunnerPlacement::new(
            session_id(SESSION),
            placement_request(profile("readonly")),
        )
        .pin_and_offer_lease(
            &enrollment,
            &initial,
            directory("/workspace/session"),
            None,
            authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        )
        .expect("the initial registration pins the placement");
        let current = enrollment
            .register(advertisement(), &catalog())
            .expect("the replacement advertisement is valid");

        assert_eq!(
            pin.placement.offer_lease(
                &enrollment,
                &retained,
                pin.grant.as_ref(),
                authorized(
                    "inspect",
                    tool_attempt_id(RETRY_ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            ),
            Err(RunnerDomainError::RegistrationChanged)
        );
        assert_ne!(retained.revision(), current.revision());
    }

    #[test]
    fn s30_enrollment_reconstitution_rejects_cross_wired_runner() {
        let mut input = enrollment_reconstitution_input();
        input.recorded_runner = runner_id(REPLACEMENT_RUNNER);

        assert_eq!(
            RunnerEnrollment::reconstitute(input),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s30_enrollment_reconstitution_rejects_cross_wired_class_inventory() {
        let mut input = enrollment_reconstitution_input();
        input.recorded_allowed_classes = BTreeSet::new();

        assert_eq!(
            RunnerEnrollment::reconstitute(input),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s30_enrollment_reconstitution_rejects_cross_wired_state() {
        let mut input = enrollment_reconstitution_input();
        input.recorded_state = RunnerEnrollmentState::Revoked;

        assert_eq!(
            RunnerEnrollment::reconstitute(input),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s30_enrollment_reconstitution_restores_registration_revision() {
        let mut input = enrollment_reconstitution_input();
        let restored_revision = RunnerGeneration::try_from_u64(2).expect("two is positive");
        input.registration_revision = Some(restored_revision);
        input.recorded_registration_revision = Some(restored_revision);
        let enrollment = RunnerEnrollment::reconstitute(input)
            .expect("the complete enrollment facts restore the registration counter");

        let registration = enrollment
            .register(advertisement(), &catalog())
            .expect("the next registration advances the restored counter");

        assert_eq!(
            registration.revision(),
            RunnerGeneration::try_from_u64(3).expect("three is positive")
        );
    }

    #[test]
    fn s30_enrollment_reconstitution_rejects_cross_wired_registration_revision() {
        let mut input = enrollment_reconstitution_input();
        input.registration_revision = Some(RunnerGeneration::one());
        input.recorded_registration_revision =
            Some(RunnerGeneration::try_from_u64(2).expect("two is positive"));

        assert_eq!(
            RunnerEnrollment::reconstitute(input),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s31_unclaimed_side_effecting_loss_is_releasable() {
        let attempt = tool_attempt_id(ATTEMPT);
        let (registration, placement, grant, batch, lease) = offered_from_batch("deploy");
        let preexisting_clone = batch.clone();
        let proof = no_execution_proof(&lease);
        let loss = lease
            .lose_unclaimed(&proof)
            .expect("proof-backed unclaimed loss is checked");
        let prepared = loss
            .retry()
            .expect("unclaimed loss carries retry authority")
            .prepare_unclaimed_attempt(batch)
            .expect("the owning batch reauthorizes the never-executed attempt");
        let (retry_batch, authorization) = prepared.into_parts();
        let replacement = placement
            .offer_retry(
                &enrollment_for_registration(&registration),
                &registration,
                grant.as_ref(),
                loss,
                authorization,
            )
            .expect("unclaimed loss retains its never-executed attempt");
        let duplicate = retry_batch
            .resume_runner_attempt(attempt)
            .expect_err("the reissued runner authority remains single-use");
        let clone_duplicate = preexisting_clone
            .resume_runner_attempt(attempt)
            .expect_err("a preexisting clone shares the reissued single-use fence");
        let retained_authorized_attempts = preexisting_clone
            .runner_authorized_attempts()
            .collect::<Vec<_>>();

        assert_eq!(
            replacement.generation(),
            RunnerGeneration::try_from_u64(2).expect("two is positive")
        );
        assert_eq!(replacement.attempt(), attempt);
        assert_eq!(retained_authorized_attempts, vec![attempt]);
        assert_eq!(
            duplicate.failure(),
            ToolBatchExecutionFailure::AttemptStageMismatch
        );
        assert_eq!(
            clone_duplicate.failure(),
            ToolBatchExecutionFailure::AttemptStageMismatch
        );
    }

    #[test]
    fn s31_unclaimed_retry_preparation_is_single_use_across_batch_copies() {
        let (_, _, _, batch, lease) = offered_from_batch("deploy");
        let retained_batch = batch.clone();
        let proof = no_execution_proof(&lease);
        let loss = lease
            .lose_unclaimed(&proof)
            .expect("proof-backed unclaimed loss is checked");
        let _prepared = loss
            .retry()
            .expect("unclaimed loss carries retry authority")
            .prepare_unclaimed_attempt(batch)
            .expect("the first batch copy consumes retry preparation authority");

        assert_eq!(
            loss.retry()
                .expect("the loss still exposes its checked lineage")
                .prepare_unclaimed_attempt(retained_batch),
            Err(RunnerDomainError::InvalidState)
        );
    }

    #[test]
    fn s31_unclaimed_retry_authority_is_rejected_by_ordinary_offer() {
        let (registration, placement, grant, batch, lease) = offered_from_batch("deploy");
        let proof = no_execution_proof(&lease);
        let loss = lease
            .lose_unclaimed(&proof)
            .expect("proof-backed unclaimed loss is checked");
        let prepared = loss
            .retry()
            .expect("unclaimed loss carries retry authority")
            .prepare_unclaimed_attempt(batch)
            .expect("the owning batch reauthorizes the never-executed attempt");
        let (_, authorization) = prepared.into_parts();

        assert_eq!(
            placement.offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                grant.as_ref(),
                authorization,
                lease_offer_request("deploy"),
            ),
            Err(RunnerDomainError::CorrelationMismatch)
        );
    }

    #[test]
    fn s31_offered_side_effecting_loss_without_proof_is_ambiguous() {
        let (_, _, _, offered) = offered("deploy", tool_attempt_id(ATTEMPT));
        let expected_attempt = offered.attempt();

        let loss = offered
            .lose()
            .expect("loss without no-execution proof stays ambiguous");

        assert_eq!(loss.retry(), None);
        assert_eq!(loss.crash_attempt(), Some(expected_attempt));
    }

    #[test]
    fn s31_claimed_pure_retry_requires_fresh_physical_attempt() {
        let expected_tool = tool("inspect");
        let retry_attempt = tool_attempt_id(RETRY_ATTEMPT);
        let (registration, placement, grant, offered) =
            offered("inspect", tool_attempt_id(ATTEMPT));
        let offered_attempt = offered.attempt();
        let correlation = offered.correlation();
        let claimed = offered
            .claim(correlation)
            .expect("the exact fence claims the offered lease");

        let loss = claimed.lose().expect("a claimed lease can be lost");
        let prepared = loss
            .retry()
            .expect("claimed pure work carries retry authority")
            .prepare_claimed_attempt(
                claimed_batch("inspect", RunnerToolEffectClass::Pure),
                retry_attempt,
            )
            .expect("retry authority produces a fresh physical attempt");
        let (retry_batch, retired_attempt, authorization) = prepared.into_parts();
        let replacement = placement
            .offer_retry(
                &enrollment_for_registration(&registration),
                &registration,
                grant.as_ref(),
                loss,
                authorization,
            )
            .expect("pure claimed work permits a fresh physical attempt");
        let duplicate_local_authority = retry_batch
            .resume_in_flight_attempt(retry_attempt)
            .expect("the replacement remains locally resumable");
        let duplicate_approved = approved_request("inspect");

        assert_eq!(
            RunnerToolAttemptAuthorization::try_new(duplicate_approved, duplicate_local_authority),
            Err(RunnerDomainError::InvalidState)
        );
        assert_eq!(retired_attempt.attempt(), offered_attempt);
        assert_eq!(
            retired_attempt.end(),
            &ToolAttemptEnd::KnownFailed {
                error: crate::ToolExecutionError::new(ToolExecutionErrorKind::CrashLost, None),
            }
        );
        assert_eq!(
            current_attempt_id(&retry_batch, request("inspect").id()),
            Some(retry_attempt)
        );
        assert_eq!(replacement.attempt(), retry_attempt);
        assert_eq!(replacement.tool(), &expected_tool);
    }

    #[test]
    fn s31_claimed_retry_rejects_cross_wired_lost_lease_correlation() {
        let (registration, placement, grant, offered) =
            offered("inspect", tool_attempt_id(ATTEMPT));
        let correlation = offered.correlation();
        let claimed = offered
            .claim(correlation)
            .expect("the exact first lease is claimed");
        let source_loss = claimed.lose().expect("the claimed pure lease may be lost");
        let prepared = source_loss
            .retry()
            .expect("the source loss carries retry authority")
            .prepare_claimed_attempt(
                claimed_batch("inspect", RunnerToolEffectClass::Pure),
                tool_attempt_id(RETRY_ATTEMPT),
            )
            .expect("the source loss prepares its exact replacement");
        let (_, _, authorization) = prepared.into_parts();
        let mut cross_wired_input = borrowed_lease_reconstitution_input(source_loss.lost());
        cross_wired_input.lease = runner_lease_id(LEASE + 1);
        cross_wired_input.recorded_correlation.lease = runner_lease_id(LEASE + 1);
        let cross_wired_loss =
            RunnerLease::reconstitute_loss(cross_wired_input, &registration, None)
                .expect("the distinct complete loss is internally consistent");

        assert_eq!(
            placement.offer_retry(
                &enrollment_for_registration(&registration),
                &registration,
                grant.as_ref(),
                cross_wired_loss,
                authorization,
            ),
            Err(RunnerDomainError::CorrelationMismatch)
        );
    }

    #[test]
    fn s31_later_retry_rejects_retired_attempt_identity() {
        let (registration, placement, grant, offered) =
            offered("inspect", tool_attempt_id(ATTEMPT));
        let retired_identity = offered.attempt();
        let first_correlation = offered.correlation();
        let first_claimed = offered
            .claim(first_correlation)
            .expect("the exact first fence claims");
        let first_loss = first_claimed.lose().expect("the first claim may be lost");
        let prepared = first_loss
            .retry()
            .expect("the first loss carries retry authority")
            .prepare_claimed_attempt(
                claimed_batch("inspect", RunnerToolEffectClass::Pure),
                tool_attempt_id(RETRY_ATTEMPT),
            )
            .expect("the first retry replaces the claimed physical attempt");
        let (retry_batch, _, authorization) = prepared.into_parts();
        let retry_lease = placement
            .offer_retry(
                &enrollment_for_registration(&registration),
                &registration,
                grant.as_ref(),
                first_loss,
                authorization,
            )
            .expect("the checked first replacement offers its successor lease");
        let retry_correlation = retry_lease.correlation();
        let retry_claimed = retry_lease
            .claim(retry_correlation)
            .expect("the exact retry fence claims");
        let retry_loss = retry_claimed
            .lose()
            .expect("the claimed retry may itself be lost");

        assert_eq!(
            retry_loss
                .retry()
                .expect("the later loss carries retry authority")
                .prepare_claimed_attempt(retry_batch, retired_identity),
            Err(RunnerDomainError::AttemptIdentityReuse),
        );
    }

    #[test]
    fn s31_claimed_retry_rejects_attempt_identity_reuse() {
        let (_, _, _, offered) = offered("sync", tool_attempt_id(ATTEMPT));
        let correlation = offered.correlation();
        let claimed = offered
            .claim(correlation)
            .expect("the exact fence claims the offered lease");

        let loss = claimed.lose().expect("a claimed lease can be lost");

        assert_eq!(
            loss.retry()
                .expect("claimed idempotent work carries retry authority")
                .prepare_claimed_attempt(
                    claimed_batch("sync", RunnerToolEffectClass::Idempotent),
                    tool_attempt_id(ATTEMPT),
                ),
            Err(RunnerDomainError::AttemptIdentityReuse)
        );
    }

    #[test]
    fn s31_claimed_retry_rejects_a_different_request() {
        let (_, _, _, offered) = offered("sync", tool_attempt_id(ATTEMPT));
        let correlation = offered.correlation();
        let claimed = offered
            .claim(correlation)
            .expect("the exact fence claims the offered lease");
        let loss = claimed.lose().expect("a claimed lease can be lost");

        assert_eq!(
            loss.retry()
                .expect("claimed idempotent work carries retry authority")
                .prepare_claimed_attempt(
                    claimed_batch("inspect", RunnerToolEffectClass::Pure),
                    tool_attempt_id(RETRY_ATTEMPT),
                ),
            Err(RunnerDomainError::CorrelationMismatch)
        );
    }

    #[test]
    fn s31_claimed_retry_rejects_cross_wired_issuing_attempt() {
        let (_, _, _, offered) = offered("sync", tool_attempt_id(ATTEMPT));
        let correlation = offered.correlation();
        let claimed = offered
            .claim(correlation)
            .expect("the exact fence claims the offered lease");
        let loss = claimed.lose().expect("a claimed lease can be lost");
        let cross_wired = claimed_batch_with_issuing_attempt(
            "sync",
            RunnerToolEffectClass::Idempotent,
            turn_attempt_id(0x7b01),
        );

        assert_eq!(
            loss.retry()
                .expect("claimed idempotent work carries retry authority")
                .prepare_claimed_attempt(cross_wired, tool_attempt_id(RETRY_ATTEMPT)),
            Err(RunnerDomainError::CorrelationMismatch)
        );
    }

    #[test]
    fn s31_unclaimed_retry_cannot_mint_a_fresh_attempt() {
        let (_, _, _, offered) = offered("inspect", tool_attempt_id(ATTEMPT));
        let proof = no_execution_proof(&offered);
        let loss = offered
            .lose_unclaimed(&proof)
            .expect("proof-backed unclaimed loss is checked");

        assert_eq!(
            loss.retry()
                .expect("unclaimed pure work carries retry authority")
                .prepare_claimed_attempt(
                    claimed_batch("inspect", RunnerToolEffectClass::Pure),
                    tool_attempt_id(RETRY_ATTEMPT),
                ),
            Err(RunnerDomainError::InvalidState)
        );
    }

    #[test]
    fn s31_claimed_retry_authority_preserves_effect_class() {
        let (_, _, _, offered) = offered("sync", tool_attempt_id(ATTEMPT));
        let correlation = offered.correlation();
        let claimed = offered
            .claim(correlation)
            .expect("the exact fence claims the offered lease");
        let loss = claimed.lose().expect("a claimed lease can be lost");
        let source_batch = claimed_batch("sync", RunnerToolEffectClass::Idempotent);
        let source_effect = current_attempt_effect_class(&source_batch, request("sync").id())
            .expect("the source batch carries its current attempt effect");
        let prepared = loss
            .retry()
            .expect("claimed idempotent work carries retry authority")
            .prepare_claimed_attempt(source_batch, tool_attempt_id(RETRY_ATTEMPT))
            .expect("retry authority preserves the source effect");
        let (_, retired_attempt, authorization) = prepared.into_parts();
        let (attempt, _) = authorization.authorized.into_parts();

        assert_eq!(retired_attempt.end(), &ToolAttemptEnd::Ambiguous);
        assert_eq!(attempt.effect_class(), source_effect);
    }

    #[test]
    fn s31_claimed_retry_rejects_standalone_same_request_authority() {
        let (registration, placement, grant, offered) =
            offered("inspect", tool_attempt_id(ATTEMPT));
        let correlation = offered.correlation();
        let claimed = offered
            .claim(correlation)
            .expect("the exact fence claims the first lease");
        let loss = claimed
            .lose()
            .expect("claimed pure work permits checked retry");

        assert_eq!(
            placement.offer_retry(
                &enrollment_for_registration(&registration),
                &registration,
                grant.as_ref(),
                loss,
                authorized(
                    "inspect",
                    tool_attempt_id(RETRY_ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
            ),
            Err(RunnerDomainError::CorrelationMismatch)
        );
    }

    #[test]
    fn s31_claimed_side_effecting_loss_requires_crash_classification() {
        let (_, _, _, offered) = offered("deploy", tool_attempt_id(ATTEMPT));
        let correlation = offered.correlation();
        let expected_attempt = correlation.dispatch.attempt();
        let claimed = offered
            .claim(correlation)
            .expect("the exact fence claims the offered lease");

        let loss = claimed.lose().expect("a claimed lease can be lost");

        assert_eq!(loss.retry(), None);
        assert_eq!(loss.crash_attempt(), Some(expected_attempt));
    }

    #[test]
    fn s31_stale_generation_cannot_claim() {
        let (_, _, _, offered) = offered("inspect", tool_attempt_id(ATTEMPT));
        let stale = RunnerLeaseCorrelation {
            generation: RunnerGeneration::try_from_u64(2).expect("two is positive"),
            ..offered.correlation()
        };

        assert_eq!(
            offered.claim(stale),
            Err(RunnerDomainError::CorrelationMismatch)
        );
    }

    #[test]
    fn s12_cross_wired_attempt_dispatch_cannot_claim() {
        let (_, _, _, offered) = offered("inspect", tool_attempt_id(ATTEMPT));
        let stale = RunnerLeaseCorrelation {
            dispatch: authorized(
                "inspect",
                tool_attempt_id(RETRY_ATTEMPT),
                RunnerToolEffectClass::Pure,
            )
            .authorized
            .correlation(),
            ..offered.correlation()
        };

        assert_eq!(
            offered.claim(stale),
            Err(RunnerDomainError::CorrelationMismatch)
        );
    }

    #[test]
    fn s31_lease_requires_matching_authorized_attempt_effect() {
        let (registration, pin) = pinned("readonly");

        assert_eq!(
            pin.placement.offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                pin.grant.as_ref(),
                authorized(
                    "inspect",
                    tool_attempt_id(RETRY_ATTEMPT),
                    RunnerToolEffectClass::SideEffecting,
                ),
                lease_offer_request("inspect"),
            ),
            Err(RunnerDomainError::CorrelationMismatch)
        );
    }

    #[test]
    fn s12_lease_requires_the_authorized_request_tool() {
        let (registration, pin) = pinned("readonly");

        assert_eq!(
            pin.placement.offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                pin.grant.as_ref(),
                authorized(
                    "deploy",
                    tool_attempt_id(RETRY_ATTEMPT),
                    RunnerToolEffectClass::SideEffecting,
                ),
                lease_offer_request("inspect"),
            ),
            Err(RunnerDomainError::CorrelationMismatch)
        );
    }

    #[test]
    fn s12_attempt_authorization_rejects_a_cross_wired_request() {
        let approved = approved_request("inspect");
        let deploy = approved_request("deploy");
        let authorized = deploy
            .prepare_attempt(
                tool_attempt_id(RETRY_ATTEMPT),
                turn_attempt_id(0x7b00),
                ToolEffectClass::ExternalEffect,
            )
            .authorize()
            .expect("the prepared fixture attempt authorizes once");

        assert_eq!(
            RunnerToolAttemptAuthorization::try_new(approved, authorized),
            Err(RunnerDomainError::CorrelationMismatch)
        );
    }

    #[test]
    fn s31_lease_reconstitution_rejects_cross_wired_fence() {
        let (registration, _, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT));
        let mut input = lease_reconstitution_input(lease);
        input.recorded_correlation = RunnerLeaseCorrelation {
            runner: runner_id(REPLACEMENT_RUNNER),
            ..input.recorded_correlation
        };

        assert_eq!(
            RunnerLease::reconstitute(input, &registration),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s31_lease_reconstitution_rejects_cross_wired_effect() {
        let (registration, _, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT));
        let mut input = lease_reconstitution_input(lease);
        input.recorded_effect = RunnerToolEffectClass::SideEffecting;

        assert_eq!(
            RunnerLease::reconstitute(input, &registration),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s31_lease_reconstitution_binds_effect_to_registration_declaration() {
        let (registration, _, _, offered) = offered("deploy", tool_attempt_id(ATTEMPT));
        let correlation = offered.correlation();
        let claimed = offered
            .claim(correlation)
            .expect("the exact side-effecting lease is claimed");
        let loss = claimed.lose().expect("the claimed lease may be lost");
        let mut input = borrowed_lease_reconstitution_input(loss.lost());
        input.effect = RunnerToolEffectClass::Idempotent;
        input.recorded_effect = RunnerToolEffectClass::Idempotent;

        assert_eq!(
            RunnerLease::reconstitute_loss(input, &registration, None),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s31_lease_reconstitution_rejects_cross_wired_authorization() {
        let (registration, _, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT));
        let mut input = lease_reconstitution_input(lease);
        input.recorded_credential_authorization = None;

        assert_eq!(
            RunnerLease::reconstitute(input, &registration),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s31_lease_reconstitution_rejects_foreign_credential_session() {
        let (registration, _, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT));
        let mut input = lease_reconstitution_input(lease);
        let authorization = input
            .credential_authorization
            .as_mut()
            .expect("the fixture lease carries credential authorization");
        authorization.session = session_id(SESSION + 1);
        input.recorded_credential_authorization = input.credential_authorization.clone();

        assert_eq!(
            RunnerLease::reconstitute(input, &registration),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s31_lease_reconstitution_rejects_foreign_credential_runner() {
        let (registration, _, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT));
        let mut input = lease_reconstitution_input(lease);
        let authorization = input
            .credential_authorization
            .as_mut()
            .expect("the fixture lease carries credential authorization");
        authorization.runner = runner_id(REPLACEMENT_RUNNER);
        input.recorded_credential_authorization = input.credential_authorization.clone();

        assert_eq!(
            RunnerLease::reconstitute(input, &registration),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s31_lease_reconstitution_rejects_foreign_credential_tool() {
        let (registration, _, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT));
        let mut input = lease_reconstitution_input(lease);
        let authorization = input
            .credential_authorization
            .as_mut()
            .expect("the fixture lease carries credential authorization");
        authorization.tool = tool("deploy");
        input.recorded_credential_authorization = input.credential_authorization.clone();
        assert_eq!(
            RunnerLease::reconstitute(input, &registration),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s31_lease_reconstitution_rejects_cross_wired_session() {
        let (registration, _, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT));
        let mut input = lease_reconstitution_input(lease);
        input.recorded_session = session_id(SESSION + 1);

        assert_eq!(
            RunnerLease::reconstitute(input, &registration),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s31_lease_reconstitution_rejects_cross_wired_state() {
        let (registration, _, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT));
        let mut input = lease_reconstitution_input(lease);
        input.recorded_state = RunnerLeaseState::Claimed;

        assert_eq!(
            RunnerLease::reconstitute(input, &registration),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s30_first_execution_pins_the_exact_runner() {
        let registration = registration();
        let placement = SessionRunnerPlacement::new(
            session_id(SESSION),
            placement_request(profile("readonly")),
        );

        let pinned = placement
            .pin_and_offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                directory("/workspace/session"),
                None,
                authorized(
                    "inspect",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            )
            .expect("the first authorized lease satisfies every requested axis");

        let expected_grant = pinned
            .grant
            .as_ref()
            .expect("profile selection creates a grant");
        let expected = SessionRunnerPlacementState::Pinned(PinnedRunnerPlacement {
            runner: runner_id(RUNNER),
            working_directory: directory("/workspace/session"),
            credential_profile: Some(profile("readonly")),
            grant_lineage: Some(RunnerCredentialGrantLineage {
                runner: expected_grant.runner,
                revision: expected_grant.revision(),
            }),
            tools: BTreeSet::from([tool("deploy"), tool("inspect"), tool("sync")]),
            runner_required_tools: BTreeSet::from([tool("deploy"), tool("sync")]),
            workspace: None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
        });
        assert_eq!(pinned.placement.state(), &expected);
        assert_eq!(expected_grant.profile(), &profile("readonly"));
    }

    #[test]
    fn s30_placement_reconstitution_accepts_raw_pinned_facts() {
        let (registration, pin) = pinned("readonly");
        let expected_state = pin.placement.state().clone();
        let input = placement_reconstitution_input(pin.placement);

        let reconstituted = SessionRunnerPlacement::reconstitute(
            input,
            session_id(SESSION),
            Some(&registration),
            None,
        )
        .expect("complete pinned facts reconstitute");

        assert_eq!(reconstituted.state(), &expected_state);
    }

    #[test]
    fn s30_placement_reconstitution_rejects_missing_grant_lineage() {
        let (registration, pin) = pinned("readonly");
        let corrupted = placement_without_grant_lineage(pin.placement);
        let input = placement_reconstitution_input(corrupted);

        assert_eq!(
            SessionRunnerPlacement::reconstitute(
                input,
                session_id(SESSION),
                Some(&registration),
                None,
            ),
            Err(RunnerDomainError::CorruptStoredFacts),
        );
    }

    #[test]
    fn s30_generation_one_profiled_placement_rejects_later_grant_revision() {
        let (registration, pin) = pinned("readonly");
        let corrupted = placement_with_grant_lineage(
            pin.placement,
            RunnerCredentialGrantLineage {
                runner: registration.runner(),
                revision: RunnerGeneration::try_from_u64(2).expect("two is positive"),
            },
        );
        let input = placement_reconstitution_input(corrupted);

        assert_eq!(
            SessionRunnerPlacement::reconstitute(
                input,
                session_id(SESSION),
                Some(&registration),
                None,
            ),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s30_placement_rejects_grant_lineage_newer_than_its_revision() {
        let (registration, pin) = pinned("readonly");
        let mut placement = pin.placement;
        placement.revision = RunnerGeneration::try_from_u64(2).expect("two is positive");
        let corrupted = placement_with_grant_lineage(
            placement,
            RunnerCredentialGrantLineage {
                runner: registration.runner(),
                revision: RunnerGeneration::try_from_u64(3).expect("three is positive"),
            },
        );
        let input = placement_reconstitution_input(corrupted);

        assert_eq!(
            SessionRunnerPlacement::reconstitute(
                input,
                session_id(SESSION),
                Some(&registration),
                None,
            ),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }
    #[test]
    fn s30_generation_one_profileless_placement_rejects_grant_lineage() {
        let registration = registration();
        let pin = SessionRunnerPlacement::new(session_id(SESSION), profileless_placement_request())
            .pin_and_offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                directory("/workspace/session"),
                None,
                automatically_authorized(
                    "inspect",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            )
            .expect("profileless execution pins without a grant");
        let corrupted = placement_with_grant_lineage(
            pin.placement,
            RunnerCredentialGrantLineage {
                runner: registration.runner(),
                revision: RunnerGeneration::one(),
            },
        );
        let input = placement_reconstitution_input(corrupted);

        assert_eq!(
            SessionRunnerPlacement::reconstitute(
                input,
                session_id(SESSION),
                Some(&registration),
                None,
            ),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s30_placement_reconstitution_rejects_cross_wired_session() {
        let (registration, pin) = pinned("readonly");
        let input = placement_reconstitution_input(pin.placement);

        assert_eq!(
            SessionRunnerPlacement::reconstitute(
                input,
                session_id(SESSION + 1),
                Some(&registration),
                None,
            ),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s30_placement_reconstitution_rejects_missing_required_tool() {
        let (registration, pin) = pinned("readonly");
        let corrupted = placement_without_required_tool(pin.placement, &tool("deploy"));
        let input = placement_reconstitution_input(corrupted);

        assert_eq!(
            SessionRunnerPlacement::reconstitute(
                input,
                session_id(SESSION),
                Some(&registration),
                None,
            ),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s30_placement_reconstitution_requires_complete_runner_only_set() {
        let (registration, mut pin) = pinned("readonly");
        omit_runner_required_tool(&mut pin.placement, &tool("deploy"));
        let input = placement_reconstitution_input(pin.placement);

        assert_eq!(
            SessionRunnerPlacement::reconstitute(
                input,
                session_id(SESSION),
                Some(&registration),
                None,
            ),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s30_reregistration_additions_do_not_widen_a_pinned_snapshot() {
        let narrow_registration = enrollment()
            .register(
                RunnerAdvertisement::new(
                    [class()],
                    [tool("inspect")],
                    [profile("readonly")],
                    [WorkspaceCapability::WorktreePerSession],
                    sandbox_profiles(),
                    [],
                ),
                &catalog(),
            )
            .expect("the narrow advertisement is allowed");
        let pin = SessionRunnerPlacement::new(
            session_id(SESSION),
            placement_request(profile("readonly")),
        )
        .pin_and_offer_lease(
            &enrollment_for_registration(&narrow_registration),
            &narrow_registration,
            directory("/workspace/session"),
            None,
            authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        )
        .expect("the narrow registration and first lease satisfy placement");
        let expanded_registration = registration();
        let reconciled = pin
            .placement
            .reconcile_registration(&expanded_registration)
            .expect("an expanded registration preserves the pin");

        assert_eq!(
            reconciled.offer_lease(
                &enrollment_for_registration(&expanded_registration),
                &expanded_registration,
                pin.grant.as_ref(),
                authorized(
                    "deploy",
                    tool_attempt_id(RETRY_ATTEMPT),
                    RunnerToolEffectClass::SideEffecting,
                ),
                lease_offer_request("deploy"),
            ),
            Err(RunnerDomainError::ToolUnavailable)
        );
    }

    #[test]
    fn s30_reregistration_omission_reconciles_to_runner_loss() {
        let (_, pin_for_offer) = pinned("readonly");
        let (_, pin_for_reconciliation) = pinned("readonly");
        let (_, pin_for_expected_state) = pinned("readonly");
        let narrowed_registration = enrollment()
            .register(
                RunnerAdvertisement::new(
                    [class()],
                    [tool("inspect"), tool("deploy")],
                    [profile("readonly")],
                    [WorkspaceCapability::WorktreePerSession],
                    sandbox_profiles(),
                    [],
                ),
                &catalog(),
            )
            .expect("the narrowed advertisement remains allowed");
        let expected = pin_for_expected_state
            .placement
            .reconcile_registration(&narrowed_registration)
            .expect("registration narrowing is explicit runner loss");

        assert_eq!(
            pin_for_offer.placement.offer_lease(
                &enrollment_for_registration(&narrowed_registration),
                &narrowed_registration,
                pin_for_offer.grant.as_ref(),
                authorized(
                    "inspect",
                    tool_attempt_id(RETRY_ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            ),
            Err(RunnerDomainError::RegistrationChanged)
        );
        assert_eq!(
            expected,
            pin_for_reconciliation
                .placement
                .reconcile_registration(&narrowed_registration)
                .expect("registration narrowing is explicit runner loss")
        );
    }

    #[test]
    fn s30_reconciliation_rejects_a_stale_registration() {
        let enrollment = enrollment();
        let retained = enrollment
            .register(advertisement(), &catalog())
            .expect("the first registration pins the complete runner snapshot");
        let pin = SessionRunnerPlacement::new(
            session_id(SESSION),
            placement_request(profile("readonly")),
        )
        .pin_and_offer_lease(
            &enrollment,
            &retained,
            directory("/workspace/session"),
            None,
            authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        )
        .expect("the first registration pins the runner");
        let current = enrollment
            .register(
                RunnerAdvertisement::new(
                    [class()],
                    [tool("inspect")],
                    [profile("readonly")],
                    [WorkspaceCapability::WorktreePerSession],
                    sandbox_profiles(),
                    [],
                ),
                &catalog(),
            )
            .expect("the narrowed successor registration is current");

        assert_eq!(
            pin.placement.reconcile_registration(&retained),
            Err(RunnerDomainError::RegistrationChanged)
        );
        assert_ne!(retained.revision(), current.revision());
    }

    #[test]
    fn s30_reconciliation_rejects_a_foreign_runner_registration() {
        let (_, pin) = pinned("readonly");
        let foreign = registration_for(runner_id(REPLACEMENT_RUNNER));

        assert_eq!(
            pin.placement.reconcile_registration(&foreign),
            Err(RunnerDomainError::CorrelationMismatch)
        );
    }

    #[test]
    fn s30_combined_tool_omission_retains_daemon_fallback() {
        let (_, pin) = pinned("readonly");
        let expected_state = pin.placement.state().clone();
        let expected_revision = pin.placement.revision();
        let narrowed_registration = enrollment()
            .register(
                RunnerAdvertisement::new(
                    [class()],
                    [tool("deploy"), tool("sync")],
                    [profile("readonly")],
                    [WorkspaceCapability::WorktreePerSession],
                    sandbox_profiles(),
                    [],
                ),
                &catalog(),
            )
            .expect("omitting the combined tool remains a valid registration");
        let reconciled = pin
            .placement
            .reconcile_registration(&narrowed_registration)
            .expect("combined-tool omission retains pinned placement");

        assert_eq!(reconciled.state(), &expected_state);
        assert_eq!(reconciled.revision(), expected_revision);
        assert_eq!(
            reconciled.offer_lease(
                &enrollment_for_registration(&narrowed_registration),
                &narrowed_registration,
                pin.grant.as_ref(),
                authorized(
                    "inspect",
                    tool_attempt_id(RETRY_ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            ),
            Err(RunnerDomainError::ToolUnavailable)
        );
    }

    #[test]
    fn s30_combined_tool_override_does_not_require_runner_advertisement() {
        let enrollment = enrollment();
        let registration = enrollment
            .register(
                RunnerAdvertisement::new(
                    [class()],
                    [tool("deploy"), tool("sync")],
                    [],
                    [WorkspaceCapability::WorktreePerSession],
                    sandbox_profiles(),
                    [],
                ),
                &catalog(),
            )
            .expect("the runner may omit the combined-locus tool");
        let mut request = profileless_placement_request();
        request.permission_overrides = RunnerToolPermissionOverrides::try_new([(
            tool("inspect"),
            RunnerToolPermissionOverride::Confirm,
        )])
        .expect("the daemon-declared combined-tool override is valid");
        let pin = SessionRunnerPlacement::new(session_id(SESSION), request)
            .pin_and_offer_lease(
                &enrollment,
                &registration,
                directory("/workspace/session"),
                None,
                authorized(
                    "deploy",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::SideEffecting,
                ),
                lease_offer_request("deploy"),
            )
            .expect("the override remains session policy while another tool dispatches");
        let unavailable = pin.placement.offer_lease(
            &enrollment,
            &registration,
            None,
            authorized(
                "inspect",
                tool_attempt_id(RETRY_ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        );

        assert_eq!(unavailable, Err(RunnerDomainError::ToolUnavailable));
    }

    #[test]
    fn s30_permission_override_rejects_tool_absent_from_daemon_catalog() {
        let enrollment = enrollment();
        let registration = enrollment
            .register(advertisement(), &catalog())
            .expect("the canonical registration is valid");
        let mut request = profileless_placement_request();
        request.permission_overrides = RunnerToolPermissionOverrides::try_new([(
            tool("future"),
            RunnerToolPermissionOverride::Confirm,
        )])
        .expect("the override map is structurally valid before catalog validation");
        let rejected = SessionRunnerPlacement::new(session_id(SESSION), request)
            .pin_and_offer_lease(
                &enrollment,
                &registration,
                directory("/workspace/session"),
                None,
                authorized(
                    "inspect",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            )
            .expect_err("a tool outside daemon policy must fail closed");

        assert_eq!(rejected, RunnerDomainError::ToolUndeclared(tool("future")));
    }

    #[test]
    fn s30_combined_tool_override_omission_retains_daemon_fallback() {
        let registration = registration();
        let mut request = placement_request(profile("readonly"));
        request.permission_overrides = RunnerToolPermissionOverrides::try_new([(
            tool("inspect"),
            RunnerToolPermissionOverride::Confirm,
        )])
        .expect("the exact combined-tool override is valid");
        let pin = SessionRunnerPlacement::new(session_id(SESSION), request)
            .pin_and_offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                directory("/workspace/session"),
                None,
                authorized(
                    "inspect",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            )
            .expect("the combined tool pins with its exact override");
        let expected_state = pin.placement.state().clone();
        let narrowed_registration = enrollment_for_registration(&registration)
            .register(
                RunnerAdvertisement::new(
                    [class()],
                    [tool("deploy"), tool("sync")],
                    [profile("readonly")],
                    [WorkspaceCapability::WorktreePerSession],
                    sandbox_profiles(),
                    [],
                ),
                &catalog(),
            )
            .expect("omitting the combined tool remains a valid registration");
        let reconciled = pin
            .placement
            .reconcile_registration(&narrowed_registration)
            .expect("the immutable override does not turn fallback into runner affinity");

        assert_eq!(reconciled.state(), &expected_state);
    }

    #[test]
    fn s30_lost_placement_cannot_offer_another_lease() {
        let (registration, mut pin) = pinned("readonly");
        let grant = pin.grant.take().expect("profile selection creates a grant");
        let lost = pin
            .placement
            .mark_runner_lost()
            .expect("the pinned runner can be marked lost");

        assert_eq!(
            lost.offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                Some(&grant),
                authorized(
                    "inspect",
                    tool_attempt_id(RETRY_ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            ),
            Err(RunnerDomainError::InvalidState)
        );
    }

    #[test]
    fn s32_replacement_is_explicit_and_advances_revision() {
        let initial = registration();
        let replacement = registration_for(runner_id(REPLACEMENT_RUNNER));
        let mut pin = SessionRunnerPlacement::new(
            session_id(SESSION),
            placement_request(profile("readonly")),
        )
        .pin_and_offer_lease(
            &enrollment_for_registration(&initial),
            &initial,
            directory("/workspace/old"),
            None,
            authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        )
        .expect("the initial registration and lease satisfy placement");
        let prior_grant = pin
            .grant
            .take()
            .expect("the selected profile creates a prior grant");
        let placement = pin
            .placement
            .mark_runner_lost()
            .expect("the pinned runner can be marked lost");

        let replaced = placement
            .replace_lost_runner(
                placement_request(profile("admin")),
                &replacement,
                directory("/workspace/new"),
                None,
                Some(prior_grant),
            )
            .expect("explicit replacement validates every new axis");

        assert_eq!(
            replaced.placement.revision(),
            RunnerGeneration::try_from_u64(2).expect("two is positive")
        );
        assert_eq!(replaced.change.before.runner, initial.runner());
        assert_eq!(replaced.change.after.runner, replacement.runner());
    }

    #[test]
    fn s32_lost_before_pin_requires_the_exact_selected_runner() {
        let selected = runner_id(RUNNER);
        let foreign = runner_id(REPLACEMENT_RUNNER);
        let capability_selected =
            SessionRunnerPlacement::new(session_id(SESSION), profileless_placement_request());
        let exact_selected =
            SessionRunnerPlacement::new(session_id(SESSION), exact_placement_request(selected));

        assert_eq!(
            capability_selected.mark_runner_lost_before_pin(selected),
            Err(RunnerDomainError::InvalidState),
        );
        assert_eq!(
            exact_selected.mark_runner_lost_before_pin(foreign),
            Err(RunnerDomainError::InvalidState),
        );
    }

    #[test]
    fn s32_pre_pin_replacement_advances_unpinned_without_pinned_facts() {
        let selected = runner_id(RUNNER);
        let replacement_registration = registration_for(runner_id(REPLACEMENT_RUNNER));
        let initial_request = exact_placement_request(selected);
        let replacement_request = exact_placement_request(replacement_registration.runner());
        let lost = SessionRunnerPlacement::new(session_id(SESSION), initial_request.clone())
            .mark_runner_lost_before_pin(selected)
            .expect("the exact selection may be lost before pinning");
        let expected_revision =
            RunnerGeneration::try_from_u64(2).expect("the fixture states revision two");
        let replacement = lost
            .replace_lost_runner_before_pin(replacement_request.clone(), &replacement_registration)
            .expect("a distinct current runner installs a successor request");

        assert_eq!(replacement.placement.revision(), expected_revision);
        assert_eq!(
            replacement.placement.state(),
            &SessionRunnerPlacementState::Unpinned,
        );
        assert_eq!(replacement.before.runner(), selected);
        assert_eq!(replacement.prior_request, initial_request);
        assert_eq!(replacement.replacement_request, replacement_request);
    }

    #[test]
    fn s32_pre_pin_replacement_requires_repository_workspace_capability() {
        let selected = runner_id(RUNNER);
        let replacement_runner = runner_id(REPLACEMENT_RUNNER);
        let replacement_registration = enrollment_for(replacement_runner)
            .register(
                RunnerAdvertisement::new(
                    [class()],
                    [tool("inspect"), tool("deploy"), tool("sync")],
                    [profile("readonly"), profile("admin")],
                    [],
                    sandbox_profiles(),
                    [RunnerRepositoryEntry::new(repository_key(), None)],
                ),
                &catalog(),
            )
            .expect("the repository inventory is valid without workspace capability");
        let lost =
            SessionRunnerPlacement::new(session_id(SESSION), exact_placement_request(selected))
                .mark_runner_lost_before_pin(selected)
                .expect("the exact selection may be lost before pinning");
        let replacement_request = SessionRunnerPlacementRequest {
            selector: RunnerSelector::Identity(replacement_runner),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: None,
            workspace: WorkspaceRequirement::RepositoryWorktree {
                repository: repository_key(),
            },
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            permission_overrides: no_permission_overrides(),
        };

        assert_eq!(
            lost.replace_lost_runner_before_pin(replacement_request, &replacement_registration,),
            Err(RunnerDomainError::WorkspaceCapabilityUnavailable),
        );
    }

    #[test]
    fn s32_connection_loss_rejects_same_runner_replacement() {
        let (registration, mut pin) = pinned("readonly");
        let prior_grant = pin.grant.take().expect("the pin carries its grant");
        let request = pin.placement.request().clone();
        let lost = pin
            .placement
            .mark_runner_lost()
            .expect("the pinned runner may be lost through its connection");

        assert_eq!(
            lost.replace_lost_runner(
                request,
                &registration,
                directory("/workspace/session"),
                None,
                Some(prior_grant),
            ),
            Err(RunnerDomainError::CorrelationMismatch),
        );
    }

    #[test]
    fn s32_registration_loss_label_does_not_authorize_same_runner_replacement() {
        let (registration, mut pin) = pinned("readonly");
        let prior_grant = pin.grant.take().expect("the pin carries its grant");
        let request = pin.placement.request().clone();
        let mut pinned = validate_placement(
            pin.placement.session(),
            pin.placement.revision(),
            &request,
            &registration,
            directory("/workspace/session"),
            None,
            WorkspaceRevisionMatch::Exact,
        )
        .expect("the fixture registration validates the pinned facts");
        pinned.grant_lineage = Some(prior_grant.lineage());
        let lost = SessionRunnerPlacement::reconstitute(
            SessionRunnerPlacementReconstitutionInput {
                session: pin.placement.session(),
                revision: pin.placement.revision(),
                request: request.clone(),
                state: SessionRunnerPlacementState::RunnerLost(
                    LostPinnedRunnerPlacement::from_stored(
                        pinned,
                        RunnerPlacementLossSource::Registration,
                    ),
                ),
                history: RunnerPlacementReconstitutionHistory::Initial,
            },
            pin.placement.session(),
            Some(&registration),
            None,
        )
        .expect("complete stored loss facts reconstitute");

        assert_eq!(
            lost.replace_lost_runner(
                request,
                &registration,
                directory("/workspace/session"),
                None,
                Some(prior_grant),
            ),
            Err(RunnerDomainError::CorrelationMismatch),
        );
    }

    #[test]
    fn s32_abandonment_retires_the_exact_lost_pre_pin_state() {
        let selected = runner_id(RUNNER);
        let lost =
            SessionRunnerPlacement::new(session_id(SESSION), exact_placement_request(selected))
                .mark_runner_lost_before_pin(selected)
                .expect("the exact selection may be lost before pinning");
        let abandoned = lost
            .abandon_lost_runner()
            .expect("the lost pre-pin selection may be abandoned");

        assert_eq!(
            abandoned.state(),
            &SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::BeforePin(
                RunnerLostBeforePin::from_stored(selected)
            ),),
        );
    }

    #[test]
    fn s32_unpinned_successor_reconstitution_requires_pre_pin_history() {
        let selected = runner_id(RUNNER);
        let replacement_registration = registration_for(runner_id(REPLACEMENT_RUNNER));
        let lost =
            SessionRunnerPlacement::new(session_id(SESSION), exact_placement_request(selected))
                .mark_runner_lost_before_pin(selected)
                .expect("the exact selection may be lost before pinning");
        let prior_revision = lost.revision();
        let prior_request = lost.request().clone();
        let replacement = lost
            .replace_lost_runner_before_pin(
                exact_placement_request(replacement_registration.runner()),
                &replacement_registration,
            )
            .expect("a distinct current runner installs a successor request");
        let initial_history = placement_reconstitution_input(replacement.placement);
        let mut replacement_history = initial_history.clone();
        replacement_history.history =
            RunnerPlacementReconstitutionHistory::PrePinReplacements(vec![
                RunnerPrePinReplacementHistory {
                    prior_revision,
                    lost_runner: selected,
                    prior_request,
                    replacement_request: initial_history.request.clone(),
                },
            ]);

        assert_eq!(
            SessionRunnerPlacement::reconstitute(initial_history, session_id(SESSION), None, None,),
            Err(RunnerDomainError::CorruptStoredFacts),
        );
        let restored = SessionRunnerPlacement::reconstitute(
            replacement_history,
            session_id(SESSION),
            None,
            None,
        )
        .expect("append-only pre-pin replacement history authenticates revision two");
        assert_eq!(restored.state(), &SessionRunnerPlacementState::Unpinned);
    }

    #[test]
    fn s32_pre_pin_reconstitution_rejects_a_truncated_predecessor_chain() {
        let second_runner = runner_id(REPLACEMENT_RUNNER);
        let third_runner = runner_id(THIRD_RUNNER);
        let second_request = exact_placement_request(second_runner);
        let third_request = exact_placement_request(third_runner);
        let input = SessionRunnerPlacementReconstitutionInput {
            session: session_id(SESSION),
            revision: RunnerGeneration::try_from_u64(3).expect("three is a positive generation"),
            request: third_request.clone(),
            state: SessionRunnerPlacementState::Unpinned,
            history: RunnerPlacementReconstitutionHistory::PrePinReplacements(vec![
                RunnerPrePinReplacementHistory {
                    prior_revision: RunnerGeneration::try_from_u64(2)
                        .expect("two is a positive generation"),
                    lost_runner: second_runner,
                    prior_request: second_request,
                    replacement_request: third_request,
                },
            ]),
        };

        assert_eq!(
            SessionRunnerPlacement::reconstitute(input, session_id(SESSION), None, None),
            Err(RunnerDomainError::CorruptStoredFacts),
        );
    }

    #[test]
    fn s32_lost_before_pin_reconstitution_requires_an_identity_selector() {
        let selected = runner_id(RUNNER);
        let input = SessionRunnerPlacementReconstitutionInput {
            session: session_id(SESSION),
            revision: RunnerGeneration::one(),
            request: profileless_placement_request(),
            state: SessionRunnerPlacementState::RunnerLostBeforePin(
                RunnerLostBeforePin::from_stored(selected),
            ),
            history: RunnerPlacementReconstitutionHistory::Initial,
        };

        assert_eq!(
            SessionRunnerPlacement::reconstitute(input, session_id(SESSION), None, None),
            Err(RunnerDomainError::CorruptStoredFacts),
        );
    }

    #[test]
    fn s32_replacement_advances_a_revoked_grant_revision() {
        let replacement = registration_for(runner_id(REPLACEMENT_RUNNER));
        let mut pin = pinned("readonly").1;
        let prior_grant = pin
            .grant
            .take()
            .expect("the selected profile creates a prior grant")
            .revoke()
            .expect("the active prior grant can be revoked");
        let lost = pin
            .placement
            .mark_runner_lost()
            .expect("the pinned placement can be marked lost");

        let replaced = lost
            .replace_lost_runner(
                placement_request(profile("readonly")),
                &replacement,
                directory("/workspace/session"),
                None,
                Some(prior_grant),
            )
            .expect("explicit replacement creates a checked successor grant");
        let replacement_grant = replaced
            .grant
            .expect("the replaced profiled placement creates a grant");

        assert_eq!(
            replacement_grant.revision(),
            RunnerGeneration::try_from_u64(2).expect("two is positive")
        );
        assert_eq!(
            replacement_grant.state(),
            CredentialProfileGrantState::Active
        );
    }

    #[test]
    fn s32_replacement_change_retains_policy_only_request_change() {
        let registration = registration();
        let replacement = registration_for(runner_id(REPLACEMENT_RUNNER));
        let before_request = placement_request(profile("readonly"));
        let after_request = SessionRunnerPlacementRequest {
            selector: RunnerSelector::Identity(replacement.runner()),
            ..before_request.clone()
        };
        let mut pin = SessionRunnerPlacement::new(session_id(SESSION), before_request.clone())
            .pin_and_offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                directory("/workspace/session"),
                None,
                authorized(
                    "inspect",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            )
            .expect("the initial request pins the selected runner");
        let prior_grant = pin
            .grant
            .take()
            .expect("profile selection creates a prior grant");
        let lost = pin
            .placement
            .mark_runner_lost()
            .expect("the pinned runner can be marked lost");

        let replaced = lost
            .replace_lost_runner(
                after_request.clone(),
                &replacement,
                directory("/workspace/session"),
                None,
                Some(prior_grant),
            )
            .expect("the exact-runner request selects the replacement runner");

        assert_eq!(
            replaced.change.after.grant_lineage,
            replaced.grant.as_ref().map(CredentialProfileGrant::lineage),
        );
        assert_eq!(replaced.change.before_request, before_request);
        assert_eq!(replaced.change.after_request, after_request);
    }

    #[test]
    fn s32_profileless_replacement_retains_grant_lineage() {
        let registration = registration();
        let replacement = registration_for(runner_id(REPLACEMENT_RUNNER));
        let mut pin = pinned("readonly").1;
        let prior_grant = pin
            .grant
            .take()
            .expect("profile selection creates a prior grant");
        let first_lost = pin
            .placement
            .mark_runner_lost()
            .expect("the profiled placement can be marked lost");
        let profileless = first_lost
            .replace_lost_runner(
                profileless_placement_request(),
                &replacement,
                directory("/workspace/session"),
                None,
                Some(prior_grant),
            )
            .expect("profileless replacement retains a terminal grant lineage");
        let tombstone = profileless
            .grant
            .expect("the prior grant lineage remains as a tombstone");
        let second_lost = profileless
            .placement
            .mark_runner_lost()
            .expect("the profileless placement can be marked lost");

        let restored = second_lost
            .replace_lost_runner(
                placement_request(profile("readonly")),
                &registration,
                directory("/workspace/session"),
                None,
                Some(tombstone),
            )
            .expect("restoring the prior profile advances its retained lineage");
        let restored_grant = restored
            .grant
            .expect("the restored profile creates an active successor grant");

        assert_eq!(
            restored_grant.revision(),
            RunnerGeneration::try_from_u64(3).expect("three is positive")
        );
        assert_eq!(restored_grant.state(), CredentialProfileGrantState::Active);
    }

    #[test]
    fn s32_profileless_placement_reconstitutes_with_exact_tombstone() {
        let replacement = registration_for(runner_id(REPLACEMENT_RUNNER));
        let mut pin = pinned("readonly").1;
        let prior_grant = pin
            .grant
            .take()
            .expect("profile selection creates a prior grant");
        let lost = pin
            .placement
            .mark_runner_lost()
            .expect("the profiled placement can be marked lost");
        let profileless = lost
            .replace_lost_runner(
                profileless_placement_request(),
                &replacement,
                directory("/workspace/session"),
                None,
                Some(prior_grant),
            )
            .expect("profileless replacement creates its terminal tombstone");
        let tombstone = profileless
            .grant
            .expect("the checked replacement returns the tombstone");
        let expected_state = profileless.placement.state().clone();
        let input = placement_reconstitution_input(profileless.placement);

        assert_eq!(
            SessionRunnerPlacement::reconstitute(
                input.clone(),
                session_id(SESSION),
                Some(&replacement),
                None,
            ),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
        let restored = SessionRunnerPlacement::reconstitute(
            input,
            session_id(SESSION),
            Some(&replacement),
            Some(&tombstone),
        )
        .expect("the exact revoked tombstone authenticates retained profileless lineage");
        assert_eq!(restored.state(), &expected_state);
    }

    #[test]
    fn s32_cross_runner_profileless_placement_reconstitutes_with_tombstone() {
        let initial = registration();
        let replacement = registration_for(runner_id(REPLACEMENT_RUNNER));
        let mut pin = SessionRunnerPlacement::new(
            session_id(SESSION),
            placement_request(profile("readonly")),
        )
        .pin_and_offer_lease(
            &enrollment_for_registration(&initial),
            &initial,
            directory("/workspace/old"),
            None,
            authorized(
                "inspect",
                tool_attempt_id(ATTEMPT),
                RunnerToolEffectClass::Pure,
            ),
            lease_offer_request("inspect"),
        )
        .expect("the initial registration and lease satisfy placement");
        let prior_grant = pin
            .grant
            .take()
            .expect("the selected profile creates a prior grant");
        let lost = pin
            .placement
            .mark_runner_lost()
            .expect("the profiled placement can be marked lost");
        let profileless = lost
            .replace_lost_runner(
                profileless_placement_request(),
                &replacement,
                directory("/workspace/new"),
                None,
                Some(prior_grant),
            )
            .expect("the replacement runner preserves the retired grant lineage");
        let tombstone = profileless
            .grant
            .expect("the checked replacement returns the prior runner tombstone");
        let expected_state = profileless.placement.state().clone();
        let input = placement_reconstitution_input(profileless.placement);

        let restored = SessionRunnerPlacement::reconstitute(
            input,
            session_id(SESSION),
            Some(&replacement),
            Some(&tombstone),
        )
        .expect("the prior runner tombstone authenticates the retained lineage");

        assert_eq!(restored.state(), &expected_state);
    }

    #[test]
    fn s32_profileless_lineage_rejects_an_omitted_tombstone() {
        let registration = registration();
        let replacement = registration_for(runner_id(REPLACEMENT_RUNNER));
        let mut pin = pinned("readonly").1;
        let prior_grant = pin
            .grant
            .take()
            .expect("profile selection creates a prior grant");
        let first_lost = pin
            .placement
            .mark_runner_lost()
            .expect("the profiled placement can be marked lost");
        let profileless = first_lost
            .replace_lost_runner(
                profileless_placement_request(),
                &replacement,
                directory("/workspace/session"),
                None,
                Some(prior_grant),
            )
            .expect("profileless replacement creates structural lineage evidence");
        let expected_lineage = profileless
            .grant
            .as_ref()
            .map(CredentialProfileGrant::lineage);
        let _omitted_tombstone = profileless
            .grant
            .expect("the profileless successor carries its tombstone");
        let second_lost = profileless
            .placement
            .mark_runner_lost()
            .expect("the profileless placement can be marked lost");

        assert_eq!(placement_grant_lineage(&second_lost), expected_lineage);
        assert_eq!(
            second_lost.replace_lost_runner(
                placement_request(profile("readonly")),
                &registration,
                directory("/workspace/session"),
                None,
                None,
            ),
            Err(RunnerDomainError::CorrelationMismatch),
        );
    }

    #[test]
    fn s32_workspace_cannot_cross_runner_ownership() {
        let registration = registration();
        let request = SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: None,
            workspace: WorkspaceRequirement::RepositoryWorktree {
                repository: repository_key(),
            },
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
        };
        let foreign_workspace = ProvisionedWorkspace {
            session: session_id(SESSION),
            placement_revision: RunnerGeneration::one(),
            runner: runner_id(REPLACEMENT_RUNNER),
            repository: Some(repository_key()),
            canonical_clone_url_digest: Some(
                CanonicalCloneUrlDigest::try_new("b".repeat(64))
                    .expect("the fixture clone URL digest is canonical"),
            ),
            credential_profile: None,
            sandbox: RunnerSandboxProfile::Ambient,
            working_directory: directory("/workspace/session"),
            relative_path: WorkspaceRelativePath::try_new(format!(
                "sessions/{}/1/repo",
                session_id(SESSION).as_uuid()
            ))
            .expect("the fixture relative path is valid"),
            manifest_id: WorkspaceManifestId::from_uuid(uuid::Uuid::from_u128(0x7b00)),
            recovery: Some(WorkspaceRecovery::Commit {
                revision: WorkspaceRevision::try_new("c".repeat(40))
                    .expect("the fixture recovery revision is canonical"),
            }),
        };

        assert_eq!(
            SessionRunnerPlacement::new(session_id(SESSION), request).pin_and_offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                directory("/workspace/session"),
                Some(foreign_workspace),
                authorized(
                    "inspect",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            ),
            Err(RunnerDomainError::WorkspaceMismatch)
        );
    }

    #[test]
    fn s32_profile_pair_resolves_automatic_without_a_value() {
        let (_, _, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT));
        let authorization = lease
            .credential_authorization()
            .expect("the selected profile authorizes the exact pair");

        assert_eq!(authorization.approval, CredentialToolApproval::Automatic);
        assert_eq!(authorization.profile, profile("readonly"));
    }

    #[test]
    fn s32_workspace_rejects_nondeterministic_relative_path() {
        let registration = registration();
        let request = SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: None,
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            permission_overrides: no_permission_overrides(),
        };
        let private_root = ProvisionedWorkspace {
            session: session_id(SESSION),
            placement_revision: RunnerGeneration::one(),
            runner: registration.runner(),
            repository: None,
            canonical_clone_url_digest: None,
            credential_profile: None,
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            working_directory: directory("/workspace/session"),
            relative_path: WorkspaceRelativePath::try_new(format!(
                "sessions/{}/1/alternate",
                session_id(SESSION).as_uuid()
            ))
            .expect("the mismatched path remains structurally safe"),
            manifest_id: WorkspaceManifestId::from_uuid(uuid::Uuid::from_u128(0x7b01)),
            recovery: None,
        };

        assert_eq!(
            SessionRunnerPlacement::new(session_id(SESSION), request).pin_and_offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                directory("/workspace/session"),
                Some(private_root),
                authorized(
                    "inspect",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            ),
            Err(RunnerDomainError::WorkspaceMismatch)
        );
    }

    #[test]
    fn s32_ambient_runner_default_rejects_a_managed_private_root() {
        let registration = registration();
        let request = SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: None,
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
        };
        let private_root = ProvisionedWorkspace {
            session: session_id(SESSION),
            placement_revision: RunnerGeneration::one(),
            runner: registration.runner(),
            repository: None,
            canonical_clone_url_digest: None,
            credential_profile: None,
            sandbox: RunnerSandboxProfile::Ambient,
            working_directory: directory("/workspace/session"),
            relative_path: WorkspaceRelativePath::try_new(format!(
                "sessions/{}/1/work",
                session_id(SESSION).as_uuid()
            ))
            .expect("the fixture relative path is valid"),
            manifest_id: WorkspaceManifestId::from_uuid(uuid::Uuid::from_u128(0x7b01)),
            recovery: None,
        };

        assert_eq!(
            SessionRunnerPlacement::new(session_id(SESSION), request).pin_and_offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                directory("/workspace/session"),
                Some(private_root),
                authorized(
                    "inspect",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            ),
            Err(RunnerDomainError::WorkspaceMismatch)
        );
    }

    #[test]
    fn s32_exact_confirm_override_precedes_ambient_pure_auto() {
        let (registration, pin) = pinned_with_confirm_override("admin");
        let lease = pin
            .placement
            .offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                pin.grant.as_ref(),
                authorized(
                    "inspect",
                    tool_attempt_id(RETRY_ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            )
            .expect("the selected profile advertises the tool");
        let authorization = lease
            .credential_authorization()
            .expect("profile selection records pair posture");

        assert_eq!(
            authorization.approval,
            CredentialToolApproval::SessionPolicy
        );
    }

    #[test]
    fn s32_exact_confirm_override_rejects_automatic_approval() {
        let (registration, pin) = pinned_with_confirm_override("admin");

        assert_eq!(
            pin.placement.offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                pin.grant.as_ref(),
                automatically_authorized(
                    "inspect",
                    tool_attempt_id(RETRY_ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            ),
            Err(RunnerDomainError::CorrelationMismatch)
        );
    }

    #[test]
    fn s32_pair_automatic_accepts_tool_policy_approval() {
        let (registration, pin) = pinned("readonly");
        let lease = pin
            .placement
            .offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                pin.grant.as_ref(),
                automatically_authorized(
                    "inspect",
                    tool_attempt_id(RETRY_ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            )
            .expect("the pair-specific automatic posture permits policy approval");

        assert_eq!(
            lease
                .credential_authorization()
                .expect("the lease freezes pair authorization")
                .approval,
            catalog().profiles[&profile("readonly")].approval_for(&tool("inspect"))
        );
    }

    #[test]
    fn s32_exact_confirm_override_rejects_session_blanket_approval() {
        let (registration, pin) = pinned_with_confirm_override("admin");

        assert_eq!(
            pin.placement.offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                pin.grant.as_ref(),
                blanket_authorized(
                    "inspect",
                    tool_attempt_id(RETRY_ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            ),
            Err(RunnerDomainError::CorrelationMismatch)
        );
    }

    #[test]
    fn s32_exact_confirm_override_accepts_user_override_approval() {
        let (registration, pin) = pinned_with_confirm_override("admin");
        let lease = pin
            .placement
            .offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                pin.grant.as_ref(),
                user_override_authorized(
                    "inspect",
                    tool_attempt_id(RETRY_ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            )
            .expect("a one-shot user override confirms the session-policy pair");

        assert_eq!(
            lease
                .credential_authorization()
                .expect("profile selection records pair posture")
                .approval,
            CredentialToolApproval::SessionPolicy
        );
    }

    #[test]
    fn s32_revocation_does_not_rewrite_an_already_offered_lease() {
        let (_, _, mut grant, offered) = offered("inspect", tool_attempt_id(ATTEMPT));
        let correlation = offered.correlation();
        let revoked = grant
            .take()
            .expect("profile selection creates a grant")
            .revoke()
            .expect("an active grant can be revoked");

        let claimed = offered
            .claim(correlation)
            .expect("an already offered lease retains its fence");

        assert_eq!(revoked.state(), CredentialProfileGrantState::Revoked);
        assert_eq!(claimed.state(), RunnerLeaseState::Claimed);
    }

    #[test]
    fn s32_revocation_gates_later_lease_creation() {
        let (registration, mut pin) = pinned("readonly");
        let revoked = pin
            .grant
            .take()
            .expect("profile selection creates a grant")
            .revoke()
            .expect("an active grant can be revoked");

        assert_eq!(
            pin.placement.offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                Some(&revoked),
                authorized(
                    "inspect",
                    tool_attempt_id(RETRY_ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            ),
            Err(RunnerDomainError::GrantRevoked)
        );
    }

    #[test]
    fn s32_repository_profile_replacement_requires_reprovisioning() {
        let expected_enrollment = enrollment();
        let registration = expected_enrollment
            .register(
                RunnerAdvertisement::new(
                    [class()],
                    [tool("inspect"), tool("deploy"), tool("sync")],
                    [profile("readonly"), profile("admin")],
                    [WorkspaceCapability::WorktreePerSession],
                    sandbox_profiles(),
                    [RunnerRepositoryEntry::new(
                        repository_key(),
                        Some(profile("readonly")),
                    )],
                ),
                &catalog(),
            )
            .expect("the repository entry binds its configured clone profile");
        let request = SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: Some(profile("readonly")),
            workspace: WorkspaceRequirement::RepositoryWorktree {
                repository: repository_key(),
            },
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
        };
        let workspace = ProvisionedWorkspace {
            session: session_id(SESSION),
            placement_revision: RunnerGeneration::one(),
            runner: registration.runner(),
            repository: Some(repository_key()),
            canonical_clone_url_digest: Some(
                CanonicalCloneUrlDigest::try_new("b".repeat(64))
                    .expect("the fixture clone URL digest is canonical"),
            ),
            credential_profile: Some(profile("readonly")),
            sandbox: RunnerSandboxProfile::Ambient,
            working_directory: directory("/workspace/session"),
            relative_path: WorkspaceRelativePath::try_new(format!(
                "sessions/{}/1/repo",
                session_id(SESSION).as_uuid()
            ))
            .expect("the fixture relative path is valid"),
            manifest_id: WorkspaceManifestId::from_uuid(uuid::Uuid::from_u128(0x7b02)),
            recovery: Some(WorkspaceRecovery::Commit {
                revision: WorkspaceRevision::try_new("c".repeat(40))
                    .expect("the fixture recovery revision is canonical"),
            }),
        };
        let mut pin = SessionRunnerPlacement::new(session_id(SESSION), request)
            .pin_and_offer_lease(
                &expected_enrollment,
                &registration,
                directory("/workspace/session"),
                Some(workspace),
                authorized(
                    "inspect",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            )
            .expect("the configured profile provisions the repository");
        let grant = pin
            .grant
            .take()
            .expect("the configured profile creates a grant");

        assert_eq!(
            pin.placement.replace_credential_profile(
                grant,
                &registration,
                profile("admin"),
                [tool("deploy")],
            ),
            Err(RunnerDomainError::CredentialProfileUnavailable)
        );
    }

    #[test]
    fn s32_replacement_binds_profile_grant_to_placement() {
        let (registration, mut pin) = pinned("readonly");
        let grant = pin.grant.take().expect("profile selection creates a grant");
        let expected_before_tools = grant.tools.clone();
        let replacement_tools = BTreeSet::from([tool("deploy")]);

        let replaced = pin
            .placement
            .replace_credential_profile(
                grant,
                &registration,
                profile("admin"),
                replacement_tools.clone(),
            )
            .expect("the explicit replacement binds placement and grant");

        assert_eq!(replaced.grant.grant.profile(), &profile("admin"));
        assert_eq!(
            replaced.grant.grant.revision(),
            RunnerGeneration::try_from_u64(2).expect("two is positive")
        );
        assert_eq!(replaced.grant.change.before_tools, expected_before_tools);
        assert_eq!(replaced.grant.change.after_tools, replacement_tools);
        assert_eq!(
            replaced.placement_change.after.credential_profile,
            Some(profile("admin"))
        );
    }

    #[test]
    fn s32_profile_replacement_rejects_stale_registration() {
        let (registration, mut pin) = pinned("readonly");
        let retained = registration.clone();
        let current = enrollment_for_registration(&registration)
            .register(advertisement(), &catalog())
            .expect("the later registration retires the retained snapshot");
        let grant = pin.grant.take().expect("profile selection creates a grant");

        assert_eq!(
            pin.placement.replace_credential_profile(
                grant,
                &retained,
                profile("admin"),
                BTreeSet::from([tool("deploy")]),
            ),
            Err(RunnerDomainError::RegistrationChanged)
        );
        assert_ne!(retained.revision(), current.revision());
    }

    #[test]
    fn s32_profile_replacement_rejects_runner_only_omission() {
        let (_, mut pin) = pinned("readonly");
        let grant = pin.grant.take().expect("profile selection creates a grant");
        let narrowed_registration = enrollment()
            .register(
                RunnerAdvertisement::new(
                    [class()],
                    [tool("inspect"), tool("deploy")],
                    [profile("readonly"), profile("admin")],
                    [WorkspaceCapability::WorktreePerSession],
                    sandbox_profiles(),
                    [],
                ),
                &catalog(),
            )
            .expect("the narrowed advertisement remains catalog-valid");

        assert_eq!(
            pin.placement.replace_credential_profile(
                grant,
                &narrowed_registration,
                profile("admin"),
                [tool("deploy")],
            ),
            Err(RunnerDomainError::RegistrationChanged)
        );
    }

    #[test]
    fn s32_grant_reconstitution_accepts_raw_active_facts() {
        let (registration, mut pin) = pinned("readonly");
        let grant = pin.grant.take().expect("profile selection creates a grant");
        let expected_profile = grant.profile().clone();
        let input = grant_reconstitution_input(grant);

        let reconstituted = CredentialProfileGrant::reconstitute(
            input,
            session_id(SESSION),
            &registration,
            RunnerSandboxProfile::Ambient,
            &no_permission_overrides(),
        )
        .expect("complete active grant facts reconstitute");

        assert_eq!(reconstituted.profile(), &expected_profile);
    }

    #[test]
    fn s32_grant_reconstitution_rejects_changed_pair_policy() {
        let (registration, mut pin) = pinned("readonly");
        let grant = pin.grant.take().expect("profile selection creates a grant");
        let mut input = grant_reconstitution_input(grant);
        input
            .approvals
            .insert(tool("inspect"), CredentialToolApproval::SessionPolicy);

        assert_eq!(
            CredentialProfileGrant::reconstitute(
                input,
                session_id(SESSION),
                &registration,
                RunnerSandboxProfile::Ambient,
                &no_permission_overrides(),
            ),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s32_grant_reconstitution_rejects_cross_wired_session() {
        let (registration, mut pin) = pinned("readonly");
        let grant = pin.grant.take().expect("profile selection creates a grant");
        let input = grant_reconstitution_input(grant);

        assert_eq!(
            CredentialProfileGrant::reconstitute(
                input,
                session_id(SESSION + 1),
                &registration,
                RunnerSandboxProfile::Ambient,
                &no_permission_overrides(),
            ),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }
    #[test]
    fn s31_profileless_confirm_rejects_policy_auto_authorization() {
        let registration = registration();
        let pin = SessionRunnerPlacement::new(session_id(SESSION), profileless_placement_request())
            .pin_and_offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                directory("/workspace/session"),
                None,
                automatically_authorized(
                    "inspect",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            )
            .expect("automatic profileless work can pin the runner");

        assert_eq!(
            pin.placement.offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                None,
                automatically_authorized(
                    "sync",
                    tool_attempt_id(RETRY_ATTEMPT),
                    RunnerToolEffectClass::Idempotent,
                ),
                lease_offer_request("sync"),
            ),
            Err(RunnerDomainError::CorrelationMismatch)
        );
    }

    #[test]
    fn s31_profileless_confirm_rejects_session_blanket_authorization() {
        let registration = registration();
        let pin = SessionRunnerPlacement::new(session_id(SESSION), profileless_placement_request())
            .pin_and_offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                directory("/workspace/session"),
                None,
                automatically_authorized(
                    "inspect",
                    tool_attempt_id(ATTEMPT),
                    RunnerToolEffectClass::Pure,
                ),
                lease_offer_request("inspect"),
            )
            .expect("automatic profileless work can pin the runner");

        assert_eq!(
            pin.placement.offer_lease(
                &enrollment_for_registration(&registration),
                &registration,
                None,
                blanket_authorized(
                    "sync",
                    tool_attempt_id(RETRY_ATTEMPT),
                    RunnerToolEffectClass::Idempotent,
                ),
                lease_offer_request("sync"),
            ),
            Err(RunnerDomainError::CorrelationMismatch)
        );
    }

    #[test]
    fn s31_lost_unclaimed_lease_reconstitutes_retry_authority() {
        let (registration, _, _, offered) = offered("inspect", tool_attempt_id(ATTEMPT));
        let proof = no_execution_proof(&offered);
        let loss = offered
            .lose_unclaimed(&proof)
            .expect("proof-backed unclaimed loss is checked");
        let expected_generation = loss
            .retry()
            .expect("unclaimed pure loss carries retry authority")
            .generation();
        let input = borrowed_lease_reconstitution_input(loss.lost());

        let restored =
            RunnerLease::reconstitute_loss(input, &registration, Some(proof.correlation().clone()))
                .expect("complete lost facts and proof restore the checked consequence");

        assert_eq!(
            restored
                .retry()
                .expect("restored unclaimed pure loss carries retry authority")
                .generation(),
            expected_generation
        );
        assert_eq!(restored.crash_attempt(), None);
    }

    #[test]
    fn s31_loss_reconstitution_restores_consumed_retry_preparation() {
        let (registration, _, _, offered) = offered("inspect", tool_attempt_id(ATTEMPT));
        let loss = offered
            .lose()
            .expect("an execution-possible pure lease carries retry authority");
        let mut input = borrowed_lease_reconstitution_input(loss.lost());
        input.retry_preparation = RunnerLeaseRetryPreparation::Prepared;
        let restored = RunnerLease::reconstitute_loss(input, &registration, None)
            .expect("the durable consumed preparation state reconstitutes");

        assert_eq!(
            restored
                .retry()
                .expect("the restored loss retains its durable identity")
                .prepare_claimed_attempt(
                    claimed_batch("inspect", RunnerToolEffectClass::Pure),
                    tool_attempt_id(RETRY_ATTEMPT),
                ),
            Err(RunnerDomainError::InvalidState)
        );
    }

    #[test]
    fn s31_lost_unclaimed_reconstitution_requires_no_execution_proof() {
        let (registration, _, _, offered) = offered("inspect", tool_attempt_id(ATTEMPT));
        let proof = no_execution_proof(&offered);
        let loss = offered
            .lose_unclaimed(&proof)
            .expect("proof-backed unclaimed loss is checked");
        let input = borrowed_lease_reconstitution_input(loss.lost());

        assert_eq!(
            RunnerLease::reconstitute_loss(input, &registration, None),
            Err(RunnerDomainError::InvalidState)
        );
    }

    #[test]
    fn s31_lost_claimed_side_effect_reconstitutes_crash_authority() {
        let (registration, _, _, offered) = offered("deploy", tool_attempt_id(ATTEMPT));
        let correlation = offered.correlation();
        let expected_attempt = offered.attempt();
        let claimed = offered
            .claim(correlation)
            .expect("the exact fence claims the offered lease");
        let loss = claimed.lose().expect("a claimed lease can be lost");
        let input = borrowed_lease_reconstitution_input(loss.lost());

        let restored = RunnerLease::reconstitute_loss(input, &registration, None)
            .expect("complete lost facts restore crash classification authority");

        assert_eq!(restored.retry(), None);
        assert_eq!(restored.crash_attempt(), Some(expected_attempt));
    }

    #[test]
    fn s31_nonlost_lease_cannot_reconstitute_a_loss_consequence() {
        let (registration, _, _, offered) = offered("inspect", tool_attempt_id(ATTEMPT));
        let input = lease_reconstitution_input(offered);

        assert_eq!(
            RunnerLease::reconstitute_loss(input, &registration, None),
            Err(RunnerDomainError::InvalidState)
        );
    }

    #[test]
    fn s32_runner_replacement_reports_complete_grant_change() {
        let (registration, mut pin) = pinned("readonly");
        let initial_grant = pin.grant.take().expect("profile selection creates a grant");
        let narrowed = pin
            .placement
            .replace_credential_profile(
                initial_grant,
                &registration,
                profile("readonly"),
                [tool("inspect")],
            )
            .expect("the explicit profile replacement narrows the grant");
        let expected_before_tools = narrowed.grant.change.after_tools.clone();
        let lost = narrowed
            .placement
            .mark_runner_lost()
            .expect("the narrowed placement can be marked lost");
        let replacement = registration_for(runner_id(REPLACEMENT_RUNNER));
        let replaced = lost
            .replace_lost_runner(
                placement_request(profile("readonly")),
                &replacement,
                directory("/workspace/session"),
                None,
                Some(narrowed.grant.grant),
            )
            .expect("runner replacement advances the narrowed grant");
        let expected_after_tools = replaced
            .grant
            .as_ref()
            .map(|grant| grant.tools.clone())
            .expect("the replacement carries its successor grant inventory");
        let change = replaced
            .grant_change
            .expect("credential-bearing replacement reports grant change facts");

        assert_eq!(
            change
                .before
                .expect("a prior grant supplies before facts")
                .tools,
            expected_before_tools
        );
        assert_eq!(
            change
                .after
                .expect("a successor grant supplies after facts")
                .tools,
            expected_after_tools
        );
    }
}
