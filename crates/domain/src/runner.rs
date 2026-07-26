//! Runner enrollment, catalog, lease, placement, and credential grants.
//!
//! The normative specification is `docs/spec/runner-protocol.md`.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
};

use crate::{
    RunnerAuthenticationId, RunnerEnrollmentId, RunnerId, RunnerLeaseId, SessionId, ToolAttemptId,
    ToolName, ToolPermissionDefault,
};

const NAME_MAX_BYTES: usize = 64;
const EXACT_VALUE_MAX_BYTES: usize = 4_096;

/// Why runner domain input or stored facts fail closed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunnerDomainError {
    Empty,
    ContainsNull,
    TooLong,
    InvalidName,
    DuplicateCapabilityClass(RunnerCapabilityClass),
    DuplicateTool(ToolName),
    DuplicateProfile(CredentialProfileName),
    DuplicateWorkspaceCapability(WorkspaceCapability),
    UndeclaredProfileTool(ToolName),
    EnrollmentRevoked,
    CapabilityClassNotAllowed(RunnerCapabilityClass),
    ToolUndeclared(ToolName),
    ToolLocusNotAllowed(ToolName),
    CredentialProfileUndeclared(CredentialProfileName),
    WorkspaceCapabilityNotAllowed(WorkspaceCapability),
    InvalidState,
    CorrelationMismatch,
    GenerationExhausted,
    AttemptIdentityReuse,
    SelectorMismatch,
    CredentialProfileUnavailable,
    WorkingDirectoryMismatch,
    WorkspaceCapabilityUnavailable,
    WorkspaceMismatch,
    ToolUnavailable,
    GrantRevoked,
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

/// A daemon-defined class used to target an unpinned runner.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunnerCapabilityClass(String);

impl RunnerCapabilityClass {
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError> {
        validate_name(value).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A runner-local credential profile represented to the daemon by name only.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CredentialProfileName(String);

impl CredentialProfileName {
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError> {
        validate_name(value).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Exact runner-interpreted working-directory text.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunnerWorkingDirectory(String);

impl RunnerWorkingDirectory {
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError> {
        validate_exact(value).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Exact repository key used for worktree provisioning.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceRepositoryKey(String);

impl WorkspaceRepositoryKey {
    pub fn try_new(value: String) -> Result<Self, RunnerDomainError> {
        validate_exact(value).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Class-or-identity runner targeting.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RunnerSelector {
    Identity(RunnerId),
    CapabilityClass(RunnerCapabilityClass),
}

/// Static nonempty admissible placement for one tool.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ToolAdmissibleLoci {
    DaemonOnly,
    RunnerOnly { selector: RunnerSelector },
    DaemonOrRunner { selector: RunnerSelector },
}

/// Required effect class for runner-admissible tool declarations.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RunnerToolEffectClass {
    Pure,
    Idempotent,
    SideEffecting,
}

impl ToolAdmissibleLoci {
    pub const fn allows_daemon(&self) -> bool {
        matches!(self, Self::DaemonOnly | Self::DaemonOrRunner { .. })
    }

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
    permission: ToolPermissionDefault,
    effect: RunnerToolEffectClass,
    loci: ToolAdmissibleLoci,
}

impl RunnerToolDeclaration {
    pub const fn new(
        name: ToolName,
        permission: ToolPermissionDefault,
        effect: RunnerToolEffectClass,
        loci: ToolAdmissibleLoci,
    ) -> Self {
        Self {
            name,
            permission,
            effect,
            loci,
        }
    }

    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    pub const fn permission(&self) -> ToolPermissionDefault {
        self.permission
    }

    pub const fn effect(&self) -> RunnerToolEffectClass {
        self.effect
    }

    pub const fn loci(&self) -> &ToolAdmissibleLoci {
        &self.loci
    }
}

/// Approval posture for an exact tool/profile pair.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CredentialToolApproval {
    Automatic,
    SessionPolicy,
}

/// Daemon-owned approval policy for one profile name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialProfilePolicy {
    name: CredentialProfileName,
    approvals: BTreeMap<ToolName, CredentialToolApproval>,
}

impl CredentialProfilePolicy {
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

    pub const fn name(&self) -> &CredentialProfileName {
        &self.name
    }

    pub fn approval_for(&self, tool: &ToolName) -> CredentialToolApproval {
        self.approvals
            .get(tool)
            .copied()
            .unwrap_or(CredentialToolApproval::SessionPolicy)
    }
}

/// Closed workspace capabilities advertised by runners.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum WorkspaceCapability {
    WorktreePerSession,
}

/// One complete daemon-authoritative catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerCatalog {
    classes: BTreeSet<RunnerCapabilityClass>,
    tools: BTreeMap<ToolName, RunnerToolDeclaration>,
    profiles: BTreeMap<CredentialProfileName, CredentialProfilePolicy>,
    workspaces: BTreeSet<WorkspaceCapability>,
}

impl RunnerCatalog {
    pub fn try_new(
        classes: impl IntoIterator<Item = RunnerCapabilityClass>,
        tools: impl IntoIterator<Item = RunnerToolDeclaration>,
        profiles: impl IntoIterator<Item = CredentialProfilePolicy>,
        workspaces: impl IntoIterator<Item = WorkspaceCapability>,
    ) -> Result<Self, RunnerDomainError> {
        let mut checked_classes = BTreeSet::new();
        for class in classes {
            if !checked_classes.insert(class.clone()) {
                return Err(RunnerDomainError::DuplicateCapabilityClass(class));
            }
        }
        let mut checked_tools = BTreeMap::new();
        for tool in tools {
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
        Ok(Self {
            classes: checked_classes,
            tools: checked_tools,
            profiles: checked_profiles,
            workspaces: checked_workspaces,
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
}

impl RunnerAdvertisement {
    pub fn new(
        classes: impl IntoIterator<Item = RunnerCapabilityClass>,
        tools: impl IntoIterator<Item = ToolName>,
        profiles: impl IntoIterator<Item = CredentialProfileName>,
        workspaces: impl IntoIterator<Item = WorkspaceCapability>,
    ) -> Self {
        Self {
            classes: classes.into_iter().collect(),
            tools: tools.into_iter().collect(),
            profiles: profiles.into_iter().collect(),
            workspaces: workspaces.into_iter().collect(),
        }
    }
}

/// Active or terminally revoked logical enrollment.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RunnerEnrollmentState {
    Active,
    Revoked,
}

/// Logical enrollment; identity never derives from machine properties.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerEnrollment {
    enrollment: RunnerEnrollmentId,
    runner: RunnerId,
    authentication: RunnerAuthenticationId,
    allowed_classes: BTreeSet<RunnerCapabilityClass>,
    state: RunnerEnrollmentState,
}

impl RunnerEnrollment {
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
        }
    }

    pub const fn enrollment(&self) -> RunnerEnrollmentId {
        self.enrollment
    }

    pub const fn runner(&self) -> RunnerId {
        self.runner
    }

    pub const fn authentication(&self) -> RunnerAuthenticationId {
        self.authentication
    }

    pub const fn state(&self) -> RunnerEnrollmentState {
        self.state
    }

    pub fn revoke(mut self) -> Result<Self, RunnerDomainError> {
        if self.state != RunnerEnrollmentState::Active {
            return Err(RunnerDomainError::InvalidState);
        }
        self.state = RunnerEnrollmentState::Revoked;
        Ok(self)
    }

    pub fn register(
        &self,
        advertisement: RunnerAdvertisement,
        catalog: &RunnerCatalog,
    ) -> Result<ValidatedRunnerRegistration, RunnerDomainError> {
        if self.state != RunnerEnrollmentState::Active {
            return Err(RunnerDomainError::EnrollmentRevoked);
        }
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
        Ok(ValidatedRunnerRegistration {
            enrollment: self.enrollment,
            runner: self.runner,
            authentication: self.authentication,
            classes: advertisement.classes,
            tools,
            profiles,
            workspaces: advertisement.workspaces,
        })
    }

    pub fn reconstitute(
        input: RunnerEnrollmentReconstitutionInput,
    ) -> Result<Self, RunnerDomainError> {
        if input.enrollment != input.recorded_enrollment
            || input.runner != input.recorded_runner
            || input.authentication != input.recorded_authentication
            || input.allowed_classes != input.recorded_allowed_classes
            || input.state != input.recorded_state
        {
            return Err(RunnerDomainError::CorruptStoredFacts);
        }
        Ok(Self {
            enrollment: input.enrollment,
            runner: input.runner,
            authentication: input.authentication,
            allowed_classes: input.allowed_classes,
            state: input.state,
        })
    }
}

/// Complete independently stored enrollment facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerEnrollmentReconstitutionInput {
    pub enrollment: RunnerEnrollmentId,
    pub recorded_enrollment: RunnerEnrollmentId,
    pub runner: RunnerId,
    pub recorded_runner: RunnerId,
    pub authentication: RunnerAuthenticationId,
    pub recorded_authentication: RunnerAuthenticationId,
    pub allowed_classes: BTreeSet<RunnerCapabilityClass>,
    pub recorded_allowed_classes: BTreeSet<RunnerCapabilityClass>,
    pub state: RunnerEnrollmentState,
    pub recorded_state: RunnerEnrollmentState,
}

/// Validated availability paired with daemon-owned policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedRunnerRegistration {
    enrollment: RunnerEnrollmentId,
    runner: RunnerId,
    authentication: RunnerAuthenticationId,
    classes: BTreeSet<RunnerCapabilityClass>,
    tools: BTreeMap<ToolName, RunnerToolDeclaration>,
    profiles: BTreeMap<CredentialProfileName, CredentialProfilePolicy>,
    workspaces: BTreeSet<WorkspaceCapability>,
}

impl ValidatedRunnerRegistration {
    pub const fn enrollment(&self) -> RunnerEnrollmentId {
        self.enrollment
    }

    pub const fn runner(&self) -> RunnerId {
        self.runner
    }

    pub const fn authentication(&self) -> RunnerAuthenticationId {
        self.authentication
    }

    pub fn satisfies(&self, selector: &RunnerSelector) -> bool {
        match selector {
            RunnerSelector::Identity(runner) => self.runner == *runner,
            RunnerSelector::CapabilityClass(class) => self.classes.contains(class),
        }
    }

    pub fn tool(&self, tool: &ToolName) -> Option<&RunnerToolDeclaration> {
        self.tools.get(tool)
    }

    pub fn profile(&self, profile: &CredentialProfileName) -> Option<&CredentialProfilePolicy> {
        self.profiles.get(profile)
    }

    pub fn supports_workspace(&self, capability: WorkspaceCapability) -> bool {
        self.workspaces.contains(&capability)
    }

    pub fn tool_names(&self) -> impl Iterator<Item = &ToolName> {
        self.tools.keys()
    }
}

/// Positive runner lease, placement, or grant generation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunnerGeneration(NonZeroU64);

impl RunnerGeneration {
    pub const fn one() -> Self {
        Self(NonZeroU64::MIN)
    }

    pub const fn try_from_u64(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }

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
    pub lease: RunnerLeaseId,
    pub runner: RunnerId,
    pub tool: ToolName,
    pub attempt: ToolAttemptId,
    pub generation: RunnerGeneration,
}

/// Runner lease stage independent of a streaming connection.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RunnerLeaseState {
    Offered,
    Claimed,
    Completed,
    LostUnclaimed,
    LostClaimed,
}

/// One fenced runner lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerLease {
    lease: RunnerLeaseId,
    attempt: ToolAttemptId,
    session: SessionId,
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
            attempt: input.attempt,
            session: input.session,
            runner: input.runner,
            tool: input.tool,
            effect: input.effect,
            credential_authorization: input.credential_authorization,
            generation: input.generation,
            state: RunnerLeaseState::Offered,
        }
    }

    pub fn correlation(&self) -> RunnerLeaseCorrelation {
        RunnerLeaseCorrelation {
            lease: self.lease,
            runner: self.runner,
            tool: self.tool.clone(),
            attempt: self.attempt,
            generation: self.generation,
        }
    }

    pub const fn state(&self) -> RunnerLeaseState {
        self.state
    }

    pub const fn generation(&self) -> RunnerGeneration {
        self.generation
    }

    pub const fn attempt(&self) -> ToolAttemptId {
        self.attempt
    }

    pub const fn tool(&self) -> &ToolName {
        &self.tool
    }

    pub const fn credential_authorization(&self) -> Option<&CredentialDispatchAuthorization> {
        self.credential_authorization.as_ref()
    }

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

    pub fn lose(mut self) -> Result<RunnerLeaseLoss, RunnerDomainError> {
        let claimed = match self.state {
            RunnerLeaseState::Offered => false,
            RunnerLeaseState::Claimed => true,
            _ => return Err(RunnerDomainError::InvalidState),
        };
        self.state = if claimed {
            RunnerLeaseState::LostClaimed
        } else {
            RunnerLeaseState::LostUnclaimed
        };
        if claimed && self.effect == RunnerToolEffectClass::SideEffecting {
            let attempt = self.attempt;
            return Ok(RunnerLeaseLoss::CrashClassificationRequired {
                lost: self,
                attempt,
            });
        }
        let generation = self
            .generation
            .checked_next()
            .ok_or(RunnerDomainError::GenerationExhausted)?;
        let claimed_attempt = claimed.then_some(self.attempt);
        Ok(RunnerLeaseLoss::RetryPermitted {
            lost: self,
            retry: RunnerLeaseRetryAuthority {
                generation,
                claimed_attempt,
            },
        })
    }

    pub fn reconstitute(input: RunnerLeaseReconstitutionInput) -> Result<Self, RunnerDomainError> {
        if input.lease.correlation() != input.recorded_correlation {
            return Err(RunnerDomainError::CorruptStoredFacts);
        }
        Ok(input.lease)
    }
}

struct ValidatedRunnerLeaseOffer {
    lease: RunnerLeaseId,
    attempt: ToolAttemptId,
    session: SessionId,
    runner: RunnerId,
    tool: ToolName,
    effect: RunnerToolEffectClass,
    credential_authorization: Option<CredentialDispatchAuthorization>,
    generation: RunnerGeneration,
}

/// Complete lease projection plus independently stored fence facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerLeaseReconstitutionInput {
    pub lease: RunnerLease,
    pub recorded_correlation: RunnerLeaseCorrelation,
}

/// Typed consequence of lease loss.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunnerLeaseLoss {
    RetryPermitted {
        lost: RunnerLease,
        retry: RunnerLeaseRetryAuthority,
    },
    CrashClassificationRequired {
        lost: RunnerLease,
        attempt: ToolAttemptId,
    },
}

impl RunnerLeaseLoss {
    pub const fn retry(&self) -> Option<&RunnerLeaseRetryAuthority> {
        match self {
            Self::RetryPermitted { retry, .. } => Some(retry),
            Self::CrashClassificationRequired { .. } => None,
        }
    }

    pub const fn crash_attempt(&self) -> Option<ToolAttemptId> {
        match self {
            Self::RetryPermitted { .. } => None,
            Self::CrashClassificationRequired { attempt, .. } => Some(*attempt),
        }
    }
}

/// Checked successor fence for one lost lease lineage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerLeaseRetryAuthority {
    generation: RunnerGeneration,
    claimed_attempt: Option<ToolAttemptId>,
}

impl RunnerLeaseRetryAuthority {
    pub const fn generation(&self) -> RunnerGeneration {
        self.generation
    }
}

/// Working-directory selection at placement.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum WorkingDirectorySelection {
    RunnerDefault,
    Exact(RunnerWorkingDirectory),
}

/// Workspace requirement at placement.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum WorkspaceRequirement {
    None,
    RepositoryWorktree { repository: WorkspaceRepositoryKey },
}

/// Runner-owned workspace; the runner field is also cleanup ownership.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProvisionedWorkspace {
    pub session: SessionId,
    pub runner: RunnerId,
    pub repository: WorkspaceRepositoryKey,
    pub working_directory: RunnerWorkingDirectory,
}

/// Complete requested placement axes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRunnerPlacementRequest {
    pub selector: RunnerSelector,
    pub working_directory: WorkingDirectorySelection,
    pub credential_profile: Option<CredentialProfileName>,
    pub workspace: WorkspaceRequirement,
}

/// Complete exact pinned facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PinnedRunnerPlacement {
    pub runner: RunnerId,
    pub working_directory: RunnerWorkingDirectory,
    pub credential_profile: Option<CredentialProfileName>,
    pub tools: BTreeSet<ToolName>,
    pub workspace: Option<ProvisionedWorkspace>,
}

/// Session affinity lifecycle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionRunnerPlacementState {
    Unpinned,
    Pinned(PinnedRunnerPlacement),
    RunnerLost(PinnedRunnerPlacement),
}

/// Session placement and affinity aggregate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRunnerPlacement {
    session: SessionId,
    revision: RunnerGeneration,
    request: SessionRunnerPlacementRequest,
    state: SessionRunnerPlacementState,
}

impl SessionRunnerPlacement {
    pub const fn new(session: SessionId, request: SessionRunnerPlacementRequest) -> Self {
        Self {
            session,
            revision: RunnerGeneration::one(),
            request,
            state: SessionRunnerPlacementState::Unpinned,
        }
    }

    pub const fn state(&self) -> &SessionRunnerPlacementState {
        &self.state
    }

    pub const fn revision(&self) -> RunnerGeneration {
        self.revision
    }

    pub fn pin(
        mut self,
        registration: &ValidatedRunnerRegistration,
        directory: RunnerWorkingDirectory,
        workspace: Option<ProvisionedWorkspace>,
    ) -> Result<SessionRunnerPin, RunnerDomainError> {
        if self.state != SessionRunnerPlacementState::Unpinned {
            return Err(RunnerDomainError::InvalidState);
        }
        let pinned = validate_placement(
            self.session,
            &self.request,
            registration,
            directory,
            workspace,
        )?;
        let grant = match pinned.credential_profile.clone() {
            Some(profile) => Some(build_grant(
                self.session,
                RunnerGeneration::one(),
                registration,
                profile,
                registration.tool_names().cloned(),
                CredentialProfileGrantState::Active,
            )?),
            None => None,
        };
        self.state = SessionRunnerPlacementState::Pinned(pinned);
        Ok(SessionRunnerPin {
            placement: self,
            grant,
        })
    }

    pub fn offer_lease(
        &self,
        registration: &ValidatedRunnerRegistration,
        grant: Option<&CredentialProfileGrant>,
        lease: RunnerLeaseId,
        attempt: ToolAttemptId,
        tool: ToolName,
        generation: RunnerGeneration,
    ) -> Result<RunnerLease, RunnerDomainError> {
        let dispatch = validate_dispatch(self, registration, grant, &tool)?;
        Ok(RunnerLease::offer_validated(ValidatedRunnerLeaseOffer {
            lease,
            attempt,
            session: self.session,
            runner: dispatch.runner,
            tool,
            effect: dispatch.effect,
            credential_authorization: dispatch.credential_authorization,
            generation,
        }))
    }

    pub fn offer_retry(
        &self,
        registration: &ValidatedRunnerRegistration,
        grant: Option<&CredentialProfileGrant>,
        loss: RunnerLeaseLoss,
        attempt: ToolAttemptId,
    ) -> Result<RunnerLease, RunnerDomainError> {
        let RunnerLeaseLoss::RetryPermitted { lost, retry } = loss else {
            return Err(RunnerDomainError::InvalidState);
        };
        let dispatch = validate_dispatch(self, registration, grant, &lost.tool)?;
        if lost.session != self.session
            || lost.runner != dispatch.runner
            || lost.effect != dispatch.effect
            || lost.credential_authorization != dispatch.credential_authorization
        {
            return Err(RunnerDomainError::CorrelationMismatch);
        }
        match retry.claimed_attempt {
            Some(claimed) if attempt == claimed => {
                return Err(RunnerDomainError::AttemptIdentityReuse);
            }
            None if attempt != lost.attempt => {
                return Err(RunnerDomainError::CorrelationMismatch);
            }
            _ => {}
        }
        Ok(RunnerLease::offer_validated(ValidatedRunnerLeaseOffer {
            lease: lost.lease,
            attempt,
            session: lost.session,
            runner: lost.runner,
            tool: lost.tool,
            effect: lost.effect,
            credential_authorization: lost.credential_authorization,
            generation: retry.generation,
        }))
    }

    pub fn mark_runner_lost(mut self) -> Result<Self, RunnerDomainError> {
        let SessionRunnerPlacementState::Pinned(pinned) = self.state else {
            return Err(RunnerDomainError::InvalidState);
        };
        self.state = SessionRunnerPlacementState::RunnerLost(pinned);
        Ok(self)
    }

    pub fn replace_lost_runner(
        self,
        request: SessionRunnerPlacementRequest,
        registration: &ValidatedRunnerRegistration,
        directory: RunnerWorkingDirectory,
        workspace: Option<ProvisionedWorkspace>,
    ) -> Result<RunnerPlacementReplacement, RunnerDomainError> {
        let SessionRunnerPlacementState::RunnerLost(before) = self.state else {
            return Err(RunnerDomainError::InvalidState);
        };
        let revision = self
            .revision
            .checked_next()
            .ok_or(RunnerDomainError::GenerationExhausted)?;
        let after = validate_placement(self.session, &request, registration, directory, workspace)?;
        let grant = match after.credential_profile.clone() {
            Some(profile) => Some(build_grant(
                self.session,
                RunnerGeneration::one(),
                registration,
                profile,
                registration.tool_names().cloned(),
                CredentialProfileGrantState::Active,
            )?),
            None => None,
        };
        Ok(RunnerPlacementReplacement {
            placement: Self {
                session: self.session,
                revision,
                request,
                state: SessionRunnerPlacementState::Pinned(after.clone()),
            },
            change: RunnerPlacementChange {
                session: self.session,
                prior_revision: self.revision,
                replacement_revision: revision,
                before,
                after,
            },
            grant,
        })
    }

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
        let Some(current_profile) = &before.credential_profile else {
            return Err(RunnerDomainError::CredentialProfileUnavailable);
        };
        if !grant.matches_selection(self.session, before.runner, current_profile) {
            return Err(RunnerDomainError::CorrelationMismatch);
        }
        if registration.runner != before.runner {
            return Err(RunnerDomainError::CorrelationMismatch);
        }
        let revision = self
            .revision
            .checked_next()
            .ok_or(RunnerDomainError::GenerationExhausted)?;
        let grant = grant.replace_for(registration, profile.clone(), tools)?;
        let mut after = before.clone();
        after.credential_profile = Some(profile.clone());
        let mut request = self.request;
        request.credential_profile = Some(profile);
        Ok(CredentialProfilePlacementReplacement {
            placement: Self {
                session: self.session,
                revision,
                request,
                state: SessionRunnerPlacementState::Pinned(after.clone()),
            },
            placement_change: RunnerPlacementChange {
                session: self.session,
                prior_revision: self.revision,
                replacement_revision: revision,
                before,
                after,
            },
            grant,
        })
    }

    pub fn reconstitute(
        self,
        registration: Option<&ValidatedRunnerRegistration>,
    ) -> Result<Self, RunnerDomainError> {
        match &self.state {
            SessionRunnerPlacementState::Unpinned if self.revision == RunnerGeneration::one() => {
                Ok(self)
            }
            SessionRunnerPlacementState::Pinned(stored)
            | SessionRunnerPlacementState::RunnerLost(stored) => {
                let registration = registration.ok_or(RunnerDomainError::CorruptStoredFacts)?;
                let checked = validate_placement(
                    self.session,
                    &self.request,
                    registration,
                    stored.working_directory.clone(),
                    stored.workspace.clone(),
                )?;
                if checked == *stored {
                    Ok(self)
                } else {
                    Err(RunnerDomainError::CorruptStoredFacts)
                }
            }
            _ => Err(RunnerDomainError::CorruptStoredFacts),
        }
    }
}

struct ValidatedRunnerDispatch {
    runner: RunnerId,
    effect: RunnerToolEffectClass,
    credential_authorization: Option<CredentialDispatchAuthorization>,
}

fn validate_dispatch(
    placement: &SessionRunnerPlacement,
    registration: &ValidatedRunnerRegistration,
    grant: Option<&CredentialProfileGrant>,
    tool: &ToolName,
) -> Result<ValidatedRunnerDispatch, RunnerDomainError> {
    let SessionRunnerPlacementState::Pinned(pinned) = &placement.state else {
        return Err(RunnerDomainError::InvalidState);
    };
    if pinned.runner != registration.runner || !pinned.tools.contains(tool) {
        return Err(RunnerDomainError::ToolUnavailable);
    }
    let declaration = registration
        .tool(tool)
        .ok_or(RunnerDomainError::ToolUnavailable)?;
    let credential_authorization = match (&pinned.credential_profile, grant) {
        (None, None) => None,
        (Some(profile), Some(grant)) => {
            Some(grant.authorization_for(placement.session, pinned.runner, profile, tool)?)
        }
        _ => return Err(RunnerDomainError::CredentialProfileUnavailable),
    };
    Ok(ValidatedRunnerDispatch {
        runner: pinned.runner,
        effect: declaration.effect,
        credential_authorization,
    })
}

fn validate_placement(
    session: SessionId,
    request: &SessionRunnerPlacementRequest,
    registration: &ValidatedRunnerRegistration,
    directory: RunnerWorkingDirectory,
    workspace: Option<ProvisionedWorkspace>,
) -> Result<PinnedRunnerPlacement, RunnerDomainError> {
    if !registration.satisfies(&request.selector) {
        return Err(RunnerDomainError::SelectorMismatch);
    }
    if request
        .credential_profile
        .as_ref()
        .is_some_and(|profile| registration.profile(profile).is_none())
    {
        return Err(RunnerDomainError::CredentialProfileUnavailable);
    }
    if let WorkingDirectorySelection::Exact(required) = &request.working_directory
        && required != &directory
    {
        return Err(RunnerDomainError::WorkingDirectoryMismatch);
    }
    match (&request.workspace, &workspace) {
        (WorkspaceRequirement::None, None) => {}
        (WorkspaceRequirement::RepositoryWorktree { repository }, Some(actual))
            if registration.supports_workspace(WorkspaceCapability::WorktreePerSession)
                && actual.session == session
                && actual.runner == registration.runner
                && &actual.repository == repository
                && actual.working_directory == directory => {}
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
        tools: registration.tools.keys().cloned().collect(),
        workspace,
    })
}

/// Successful first pin with its optional runner-bound credential grant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRunnerPin {
    pub placement: SessionRunnerPlacement,
    pub grant: Option<CredentialProfileGrant>,
}

/// Successful explicit placement replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerPlacementReplacement {
    pub placement: SessionRunnerPlacement,
    pub change: RunnerPlacementChange,
    pub grant: Option<CredentialProfileGrant>,
}

/// Complete before-and-after facts for frontier injection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerPlacementChange {
    pub session: SessionId,
    pub prior_revision: RunnerGeneration,
    pub replacement_revision: RunnerGeneration,
    pub before: PinnedRunnerPlacement,
    pub after: PinnedRunnerPlacement,
}

/// One explicit profile/grant replacement bound to pinned placement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialProfilePlacementReplacement {
    pub placement: SessionRunnerPlacement,
    pub placement_change: RunnerPlacementChange,
    pub grant: CredentialProfileGrantReplacement,
}

/// Active or terminally revoked credential grant.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CredentialProfileGrantState {
    Active,
    Revoked,
}

/// Daemon grant snapshot for one runner-local profile.
#[derive(Clone, Debug, Eq, PartialEq)]
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
    fn matches_selection(
        &self,
        session: SessionId,
        runner: RunnerId,
        profile: &CredentialProfileName,
    ) -> bool {
        self.state == CredentialProfileGrantState::Active
            && self.session == session
            && self.runner == runner
            && &self.profile == profile
    }

    pub const fn state(&self) -> CredentialProfileGrantState {
        self.state
    }

    pub const fn revision(&self) -> RunnerGeneration {
        self.revision
    }

    pub const fn profile(&self) -> &CredentialProfileName {
        &self.profile
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

    pub fn revoke(mut self) -> Result<Self, RunnerDomainError> {
        if self.state != CredentialProfileGrantState::Active {
            return Err(RunnerDomainError::InvalidState);
        }
        self.state = CredentialProfileGrantState::Revoked;
        Ok(self)
    }

    pub fn reconstitute(
        self,
        expected_session: SessionId,
        registration: &ValidatedRunnerRegistration,
    ) -> Result<Self, RunnerDomainError> {
        if self.session != expected_session {
            return Err(RunnerDomainError::CorruptStoredFacts);
        }
        let checked = build_grant(
            self.session,
            self.revision,
            registration,
            self.profile.clone(),
            self.tools.clone(),
            self.state,
        )?;
        if checked == self {
            Ok(self)
        } else {
            Err(RunnerDomainError::CorruptStoredFacts)
        }
    }
}

fn build_grant(
    session: SessionId,
    revision: RunnerGeneration,
    registration: &ValidatedRunnerRegistration,
    profile: CredentialProfileName,
    tools: impl IntoIterator<Item = ToolName>,
    state: CredentialProfileGrantState,
) -> Result<CredentialProfileGrant, RunnerDomainError> {
    let policy = registration
        .profile(&profile)
        .ok_or(RunnerDomainError::CredentialProfileUnavailable)?;
    let tools: BTreeSet<_> = tools.into_iter().collect();
    if tools.iter().any(|tool| registration.tool(tool).is_none()) {
        return Err(RunnerDomainError::ToolUnavailable);
    }
    let approvals = tools
        .iter()
        .map(|tool| (tool.clone(), policy.approval_for(tool)))
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
    pub session: SessionId,
    pub runner: RunnerId,
    pub grant_revision: RunnerGeneration,
    pub profile: CredentialProfileName,
    pub tool: ToolName,
    pub approval: CredentialToolApproval,
}

/// Successful forward-only credential grant replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialProfileGrantReplacement {
    pub grant: CredentialProfileGrant,
    pub change: CredentialProfileChange,
}

/// Complete before-and-after profile/tool facts for frontier injection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialProfileChange {
    pub session: SessionId,
    pub prior_revision: RunnerGeneration,
    pub replacement_revision: RunnerGeneration,
    pub before_profile: CredentialProfileName,
    pub after_profile: CredentialProfileName,
    pub before_tools: BTreeSet<ToolName>,
    pub after_tools: BTreeSet<ToolName>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        runner_authentication_id, runner_enrollment_id, runner_id, runner_lease_id, session_id,
        tool_attempt_id,
    };

    const ENROLLMENT: u128 = 0x7100;
    const RUNNER: u128 = 0x7200;
    const REPLACEMENT_RUNNER: u128 = 0x7201;
    const AUTHENTICATION: u128 = 0x7300;
    const LEASE: u128 = 0x7400;
    const ATTEMPT: u128 = 0x7500;
    const RETRY_ATTEMPT: u128 = 0x7501;
    const SESSION: u128 = 0x7600;

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

    fn catalog() -> RunnerCatalog {
        let inspect = RunnerToolDeclaration::new(
            tool("inspect"),
            ToolPermissionDefault::Auto,
            RunnerToolEffectClass::Pure,
            ToolAdmissibleLoci::DaemonOrRunner {
                selector: RunnerSelector::CapabilityClass(class()),
            },
        );
        let deploy = RunnerToolDeclaration::new(
            tool("deploy"),
            ToolPermissionDefault::Confirm,
            RunnerToolEffectClass::SideEffecting,
            ToolAdmissibleLoci::RunnerOnly {
                selector: RunnerSelector::CapabilityClass(class()),
            },
        );
        let sync = RunnerToolDeclaration::new(
            tool("sync"),
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
        )
        .expect("the canonical catalog is internally consistent")
    }

    fn registration_for(runner: RunnerId) -> ValidatedRunnerRegistration {
        let enrollment = RunnerEnrollment::new(
            runner_enrollment_id(ENROLLMENT),
            runner,
            runner_authentication_id(AUTHENTICATION),
            [class()],
        );
        enrollment
            .register(
                RunnerAdvertisement::new(
                    [class()],
                    [tool("inspect"), tool("deploy"), tool("sync")],
                    [profile("readonly"), profile("admin")],
                    [WorkspaceCapability::WorktreePerSession],
                ),
                &catalog(),
            )
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
        }
    }

    fn directory(value: &str) -> RunnerWorkingDirectory {
        RunnerWorkingDirectory::try_new(value.to_owned())
            .expect("fixture working directories are valid")
    }

    fn pinned(profile_name: &str) -> (ValidatedRunnerRegistration, SessionRunnerPin) {
        let registration = registration();
        let pin = SessionRunnerPlacement::new(
            session_id(SESSION),
            placement_request(profile(profile_name)),
        )
        .pin(&registration, directory("/workspace/session"), None)
        .expect("the registration satisfies placement");
        (registration, pin)
    }

    fn offered(
        tool_name: &str,
        attempt: ToolAttemptId,
        generation: RunnerGeneration,
    ) -> (ValidatedRunnerRegistration, SessionRunnerPin, RunnerLease) {
        let (registration, pin) = pinned("readonly");
        let lease = pin
            .placement
            .offer_lease(
                &registration,
                pin.grant.as_ref(),
                runner_lease_id(LEASE),
                attempt,
                tool(tool_name),
                generation,
            )
            .expect("pinned placement authorizes the fixture tool");
        (registration, pin, lease)
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
            state: RunnerEnrollmentState::Active,
            recorded_state: RunnerEnrollmentState::Active,
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
    fn s30_inv042_catalog_rejects_duplicate_capability_class() {
        assert_eq!(
            RunnerCatalog::try_new([class(), class()], [], [], []),
            Err(RunnerDomainError::DuplicateCapabilityClass(class()))
        );
    }

    #[test]
    fn s30_inv042_catalog_rejects_duplicate_workspace_capability() {
        assert_eq!(
            RunnerCatalog::try_new(
                [],
                [],
                [],
                [
                    WorkspaceCapability::WorktreePerSession,
                    WorkspaceCapability::WorktreePerSession,
                ],
            ),
            Err(RunnerDomainError::DuplicateWorkspaceCapability(
                WorkspaceCapability::WorktreePerSession
            ))
        );
    }

    #[test]
    fn s30_inv001_logical_enrollment_retains_distinct_typed_identities() {
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
    }

    #[test]
    fn s30_inv042_unknown_advertised_tool_rejects_the_complete_registration() {
        let enrollment = RunnerEnrollment::new(
            runner_enrollment_id(ENROLLMENT),
            runner_id(RUNNER),
            runner_authentication_id(AUTHENTICATION),
            [class()],
        );
        let advertisement = RunnerAdvertisement::new([class()], [tool("unknown")], [], []);

        assert_eq!(
            enrollment.register(advertisement, &catalog()),
            Err(RunnerDomainError::ToolUndeclared(tool("unknown")))
        );
    }

    #[test]
    fn s30_inv042_catalog_rejects_tool_selector_for_undeclared_class() {
        let declaration = RunnerToolDeclaration::new(
            tool("specialized"),
            ToolPermissionDefault::Auto,
            RunnerToolEffectClass::Pure,
            ToolAdmissibleLoci::RunnerOnly {
                selector: RunnerSelector::CapabilityClass(class()),
            },
        );

        assert_eq!(
            RunnerCatalog::try_new([], [declaration], [], []),
            Err(RunnerDomainError::CapabilityClassNotAllowed(class()))
        );
    }

    #[test]
    fn s30_inv042_advertised_class_requires_enrollment_and_catalog_authority() {
        let enrollment = RunnerEnrollment::new(
            runner_enrollment_id(ENROLLMENT),
            runner_id(RUNNER),
            runner_authentication_id(AUTHENTICATION),
            [class()],
        );
        let catalog = RunnerCatalog::try_new([], [], [], [])
            .expect("the empty catalog is internally consistent");
        let advertisement = RunnerAdvertisement::new([class()], [], [], []);

        assert_eq!(
            enrollment.register(advertisement, &catalog),
            Err(RunnerDomainError::CapabilityClassNotAllowed(class()))
        );
    }

    #[test]
    fn s30_inv042_daemon_only_tool_rejects_the_complete_registration() {
        let enrollment = RunnerEnrollment::new(
            runner_enrollment_id(ENROLLMENT),
            runner_id(RUNNER),
            runner_authentication_id(AUTHENTICATION),
            [class()],
        );
        let daemon_only = RunnerToolDeclaration::new(
            tool("daemon"),
            ToolPermissionDefault::Auto,
            RunnerToolEffectClass::Pure,
            ToolAdmissibleLoci::DaemonOnly,
        );
        let catalog = RunnerCatalog::try_new([], [daemon_only], [], [])
            .expect("the daemon-only declaration is internally consistent");
        let advertisement = RunnerAdvertisement::new([], [tool("daemon")], [], []);

        assert_eq!(
            enrollment.register(advertisement, &catalog),
            Err(RunnerDomainError::ToolLocusNotAllowed(tool("daemon")))
        );
    }

    #[test]
    fn s30_inv042_tool_selector_must_match_advertised_runner_capability() {
        let enrollment = RunnerEnrollment::new(
            runner_enrollment_id(ENROLLMENT),
            runner_id(RUNNER),
            runner_authentication_id(AUTHENTICATION),
            [class()],
        );
        let declaration = RunnerToolDeclaration::new(
            tool("specialized"),
            ToolPermissionDefault::Auto,
            RunnerToolEffectClass::Pure,
            ToolAdmissibleLoci::RunnerOnly {
                selector: RunnerSelector::Identity(runner_id(REPLACEMENT_RUNNER)),
            },
        );
        let catalog = RunnerCatalog::try_new([], [declaration], [], [])
            .expect("the identity-targeted declaration is internally consistent");
        let advertisement = RunnerAdvertisement::new([], [tool("specialized")], [], []);

        assert_eq!(
            enrollment.register(advertisement, &catalog),
            Err(RunnerDomainError::ToolLocusNotAllowed(tool("specialized")))
        );
    }

    #[test]
    fn s30_inv042_revoked_enrollment_cannot_register() {
        let enrollment = RunnerEnrollment::new(
            runner_enrollment_id(ENROLLMENT),
            runner_id(RUNNER),
            runner_authentication_id(AUTHENTICATION),
            [class()],
        )
        .revoke()
        .expect("an active enrollment can be revoked");

        assert_eq!(
            enrollment.register(RunnerAdvertisement::new([], [], [], []), &catalog()),
            Err(RunnerDomainError::EnrollmentRevoked)
        );
    }

    #[test]
    fn s30_inv042_registration_attaches_daemon_policy_not_runner_policy() {
        let registration = registration();
        let declaration = registration
            .tool(&tool("deploy"))
            .expect("the advertised tool is validated");

        assert_eq!(declaration.permission(), ToolPermissionDefault::Confirm);
        assert_eq!(declaration.effect(), RunnerToolEffectClass::SideEffecting);
    }

    #[test]
    fn s30_inv001_enrollment_reconstitution_rejects_cross_wired_runner() {
        let mut input = enrollment_reconstitution_input();
        input.recorded_runner = runner_id(REPLACEMENT_RUNNER);

        assert_eq!(
            RunnerEnrollment::reconstitute(input),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s30_inv042_enrollment_reconstitution_rejects_cross_wired_class_inventory() {
        let mut input = enrollment_reconstitution_input();
        input.recorded_allowed_classes = BTreeSet::new();

        assert_eq!(
            RunnerEnrollment::reconstitute(input),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s30_inv042_enrollment_reconstitution_rejects_cross_wired_state() {
        let mut input = enrollment_reconstitution_input();
        input.recorded_state = RunnerEnrollmentState::Revoked;

        assert_eq!(
            RunnerEnrollment::reconstitute(input),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s31_inv043_unclaimed_side_effecting_loss_is_releasable() {
        let attempt = tool_attempt_id(ATTEMPT);
        let (registration, pin, lease) = offered("deploy", attempt, RunnerGeneration::one());

        let loss = lease.lose().expect("an offered lease can be lost");
        let replacement = pin
            .placement
            .offer_retry(&registration, pin.grant.as_ref(), loss, attempt)
            .expect("unclaimed loss retains its never-executed attempt");

        assert_eq!(
            replacement.generation(),
            RunnerGeneration::try_from_u64(2).expect("two is positive")
        );
        assert_eq!(replacement.attempt(), attempt);
    }

    #[test]
    fn s31_inv004_inv043_claimed_pure_retry_requires_fresh_physical_attempt() {
        let expected_tool = tool("inspect");
        let retry_attempt = tool_attempt_id(RETRY_ATTEMPT);
        let (registration, pin, offered) =
            offered("inspect", tool_attempt_id(ATTEMPT), RunnerGeneration::one());
        let correlation = offered.correlation();
        let claimed = offered
            .claim(correlation)
            .expect("the exact fence claims the offered lease");

        let loss = claimed.lose().expect("a claimed lease can be lost");
        let replacement = pin
            .placement
            .offer_retry(&registration, pin.grant.as_ref(), loss, retry_attempt)
            .expect("pure claimed work permits a fresh physical attempt");

        assert_eq!(replacement.attempt(), retry_attempt);
        assert_eq!(replacement.tool(), &expected_tool);
    }

    #[test]
    fn s31_inv004_inv043_claimed_retry_rejects_attempt_identity_reuse() {
        let (registration, pin, offered) =
            offered("sync", tool_attempt_id(ATTEMPT), RunnerGeneration::one());
        let correlation = offered.correlation();
        let claimed = offered
            .claim(correlation)
            .expect("the exact fence claims the offered lease");

        let loss = claimed.lose().expect("a claimed lease can be lost");

        assert_eq!(
            pin.placement.offer_retry(
                &registration,
                pin.grant.as_ref(),
                loss,
                tool_attempt_id(ATTEMPT),
            ),
            Err(RunnerDomainError::AttemptIdentityReuse)
        );
    }

    #[test]
    fn s31_inv025_inv026_inv043_claimed_side_effecting_loss_requires_crash_classification() {
        let (_, _, offered) = offered("deploy", tool_attempt_id(ATTEMPT), RunnerGeneration::one());
        let correlation = offered.correlation();
        let expected_attempt = correlation.attempt;
        let claimed = offered
            .claim(correlation)
            .expect("the exact fence claims the offered lease");

        let loss = claimed.lose().expect("a claimed lease can be lost");

        assert_eq!(loss.retry(), None);
        assert_eq!(loss.crash_attempt(), Some(expected_attempt));
    }

    #[test]
    fn s31_inv021_inv043_stale_generation_cannot_claim() {
        let (_, _, offered) = offered(
            "inspect",
            tool_attempt_id(ATTEMPT),
            RunnerGeneration::try_from_u64(2).expect("two is positive"),
        );
        let stale = RunnerLeaseCorrelation {
            generation: RunnerGeneration::one(),
            ..offered.correlation()
        };

        assert_eq!(
            offered.claim(stale),
            Err(RunnerDomainError::CorrelationMismatch)
        );
    }

    #[test]
    fn s31_inv043_lease_reconstitution_rejects_cross_wired_fence() {
        let (_, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT), RunnerGeneration::one());
        let recorded_correlation = RunnerLeaseCorrelation {
            runner: runner_id(REPLACEMENT_RUNNER),
            ..lease.correlation()
        };

        assert_eq!(
            RunnerLease::reconstitute(RunnerLeaseReconstitutionInput {
                lease,
                recorded_correlation,
            }),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s30_inv044_first_execution_pins_the_exact_runner() {
        let registration = registration();
        let placement = SessionRunnerPlacement::new(
            session_id(SESSION),
            placement_request(profile("readonly")),
        );

        let pinned = placement
            .pin(&registration, directory("/workspace/session"), None)
            .expect("the registration satisfies every requested axis");

        let expected = SessionRunnerPlacementState::Pinned(PinnedRunnerPlacement {
            runner: runner_id(RUNNER),
            working_directory: directory("/workspace/session"),
            credential_profile: Some(profile("readonly")),
            tools: BTreeSet::from([tool("deploy"), tool("inspect"), tool("sync")]),
            workspace: None,
        });
        assert_eq!(pinned.placement.state(), &expected);
        assert_eq!(
            pinned
                .grant
                .as_ref()
                .expect("profile selection creates a grant")
                .profile(),
            &profile("readonly")
        );
    }

    #[test]
    fn s30_inv044_lost_placement_cannot_offer_another_lease() {
        let (registration, mut pin) = pinned("readonly");
        let grant = pin.grant.take().expect("profile selection creates a grant");
        let lost = pin
            .placement
            .mark_runner_lost()
            .expect("the pinned runner can be marked lost");

        assert_eq!(
            lost.offer_lease(
                &registration,
                Some(&grant),
                runner_lease_id(LEASE),
                tool_attempt_id(ATTEMPT),
                tool("inspect"),
                RunnerGeneration::one(),
            ),
            Err(RunnerDomainError::InvalidState)
        );
    }

    #[test]
    fn s32_inv044_replacement_is_explicit_and_advances_revision() {
        let initial = registration();
        let replacement = registration_for(runner_id(REPLACEMENT_RUNNER));
        let placement = SessionRunnerPlacement::new(
            session_id(SESSION),
            placement_request(profile("readonly")),
        )
        .pin(&initial, directory("/workspace/old"), None)
        .expect("the initial registration satisfies placement")
        .placement
        .mark_runner_lost()
        .expect("the pinned runner can be marked lost");

        let replaced = placement
            .replace_lost_runner(
                placement_request(profile("admin")),
                &replacement,
                directory("/workspace/new"),
                None,
            )
            .expect("explicit replacement validates every new axis");

        assert_eq!(
            replaced.placement.revision(),
            RunnerGeneration::try_from_u64(2).expect("two is positive")
        );
        assert_eq!(replaced.change.before.runner, runner_id(RUNNER));
        assert_eq!(replaced.change.after.runner, runner_id(REPLACEMENT_RUNNER));
    }

    #[test]
    fn s32_inv044_workspace_cannot_cross_runner_ownership() {
        let registration = registration();
        let request = SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: None,
            workspace: WorkspaceRequirement::RepositoryWorktree {
                repository: WorkspaceRepositoryKey::try_new("signalbox".to_owned())
                    .expect("the repository key is valid"),
            },
        };
        let foreign_workspace = ProvisionedWorkspace {
            session: session_id(SESSION),
            runner: runner_id(REPLACEMENT_RUNNER),
            repository: WorkspaceRepositoryKey::try_new("signalbox".to_owned())
                .expect("the repository key is valid"),
            working_directory: directory("/workspace/session"),
        };

        assert_eq!(
            SessionRunnerPlacement::new(session_id(SESSION), request).pin(
                &registration,
                directory("/workspace/session"),
                Some(foreign_workspace),
            ),
            Err(RunnerDomainError::WorkspaceMismatch)
        );
    }

    #[test]
    fn s32_inv035_inv045_profile_pair_resolves_automatic_without_a_value() {
        let (_, _, lease) = offered("inspect", tool_attempt_id(ATTEMPT), RunnerGeneration::one());
        let authorization = lease
            .credential_authorization()
            .expect("the selected profile authorizes the exact pair");

        assert_eq!(authorization.approval, CredentialToolApproval::Automatic);
        assert_eq!(authorization.profile, profile("readonly"));
    }

    #[test]
    fn s32_inv045_pair_session_policy_overrides_tool_only_auto_default() {
        let (registration, pin) = pinned("admin");
        let lease = pin
            .placement
            .offer_lease(
                &registration,
                pin.grant.as_ref(),
                runner_lease_id(LEASE),
                tool_attempt_id(ATTEMPT),
                tool("inspect"),
                RunnerGeneration::one(),
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
    fn s32_inv045_revocation_does_not_rewrite_an_already_offered_lease() {
        let (_, mut pin, offered) =
            offered("inspect", tool_attempt_id(ATTEMPT), RunnerGeneration::one());
        let correlation = offered.correlation();
        let revoked = pin
            .grant
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
    fn s32_inv044_inv045_revocation_gates_later_lease_creation() {
        let (registration, mut pin) = pinned("readonly");
        let revoked = pin
            .grant
            .take()
            .expect("profile selection creates a grant")
            .revoke()
            .expect("an active grant can be revoked");

        assert_eq!(
            pin.placement.offer_lease(
                &registration,
                Some(&revoked),
                runner_lease_id(LEASE),
                tool_attempt_id(ATTEMPT),
                tool("inspect"),
                RunnerGeneration::one(),
            ),
            Err(RunnerDomainError::GrantRevoked)
        );
    }

    #[test]
    fn s32_inv044_inv045_replacement_binds_profile_grant_to_placement() {
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
    fn s32_inv045_grant_reconstitution_rejects_changed_pair_policy() {
        let (registration, mut pin) = pinned("readonly");
        let mut grant = pin.grant.take().expect("profile selection creates a grant");
        grant
            .approvals
            .insert(tool("inspect"), CredentialToolApproval::SessionPolicy);

        assert_eq!(
            grant.reconstitute(session_id(SESSION), &registration),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }

    #[test]
    fn s32_inv045_grant_reconstitution_rejects_cross_wired_session() {
        let (registration, mut pin) = pinned("readonly");
        let grant = pin.grant.take().expect("profile selection creates a grant");

        assert_eq!(
            grant.reconstitute(session_id(SESSION + 1), &registration),
            Err(RunnerDomainError::CorruptStoredFacts)
        );
    }
}
