//! Runner enrollment for `docs/spec/runner-protocol.md`.

use super::catalog::{
    CredentialProfilePolicy, RunnerAdvertisement, RunnerCatalog, RunnerRepositoryEntry,
    RunnerSandboxProfile, RunnerToolDeclaration, WorkspaceCapability,
};
use super::names::{
    CredentialProfileName, RunnerCapabilityClass, RunnerDomainError, RunnerGeneration,
    RunnerSelector, WorkspaceRepositoryKey,
};
use crate::{RunnerAuthenticationId, RunnerEnrollmentId, RunnerId, ToolName};
use std::{
    collections::BTreeMap, collections::BTreeSet, sync::Arc, sync::atomic::AtomicBool,
    sync::atomic::AtomicU64, sync::atomic::Ordering,
};

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
    pub(super) enrollment: RunnerEnrollmentId,
    pub(super) runner: RunnerId,
    pub(super) authentication: RunnerAuthenticationId,
    pub(super) allowed_classes: BTreeSet<RunnerCapabilityClass>,
    pub(super) state: RunnerEnrollmentState,
    pub(super) registration_revision: Arc<AtomicU64>,
    pub(super) registration_active: Arc<AtomicBool>,
    pub(super) registration_preparation: Arc<AtomicBool>,
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

    pub(super) fn authorizes(
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
    pub(super) enrollment: RunnerEnrollmentId,
    pub(super) runner: RunnerId,
    pub(super) authentication: RunnerAuthenticationId,
    pub(super) catalog_tools: BTreeSet<ToolName>,
    classes: BTreeSet<RunnerCapabilityClass>,
    pub(super) tools: BTreeMap<ToolName, RunnerToolDeclaration>,
    profiles: BTreeMap<CredentialProfileName, CredentialProfilePolicy>,
    workspaces: BTreeSet<WorkspaceCapability>,
    sandboxes: BTreeSet<RunnerSandboxProfile>,
    repositories: BTreeMap<WorkspaceRepositoryKey, RunnerRepositoryEntry>,
    revision: RunnerGeneration,
    pub(super) current_revision: Arc<AtomicU64>,
    pub(super) enrollment_active: Arc<AtomicBool>,
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

    pub(super) fn is_current(&self) -> bool {
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
