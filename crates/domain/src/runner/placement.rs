//! Runner placement for `docs/spec/runner-protocol.md`.

use super::catalog::{
    CredentialToolApproval, RunnerSandboxProfile, RunnerToolEffectClass,
    RunnerToolPermissionOverrides, ToolAdmissibleLoci, WorkspaceCapability, tool_effect_class,
};
use super::credential_grant::{
    CredentialDispatchAuthorization, CredentialProfileGrant, CredentialProfileGrantReplacement,
    CredentialProfileGrantState, RunnerApprovalPolicy, RunnerCredentialGrantChange, build_grant,
    resolve_runner_approval, successor_grant,
};
use super::enrollment::{RunnerEnrollment, ValidatedRunnerRegistration};
use super::lease::{
    RunnerLease, RunnerLeaseLoss, RunnerLeaseOfferRequest, RunnerRetryAttemptEvidence,
    RunnerToolAttemptAuthorization, ValidatedRunnerLeaseOffer,
};
use super::names::{
    CanonicalCloneUrlDigest, CredentialProfileName, RunnerDomainError, RunnerGeneration,
    RunnerSelector, RunnerWorkingDirectory, WorkspaceRecovery, WorkspaceRelativePath,
    WorkspaceRepositoryKey,
};
use crate::{
    RunnerId, SessionId, ToolAttemptDispatchCorrelation, ToolDecisionSource, ToolName,
    WorkspaceManifestId,
};
use std::collections::BTreeSet;

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
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
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
/// whether same-runner recovery is admissible.
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
    pub(super) pinned: PinnedRunnerPlacement,
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
    pub(super) session: SessionId,
    pub(super) revision: RunnerGeneration,
    pub(super) request: SessionRunnerPlacementRequest,
    pub(super) state: SessionRunnerPlacementState,
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
        if registration.runner == before.runner
            && lost.source != RunnerPlacementLossSource::Registration
        {
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
pub(super) enum WorkspaceRevisionMatch {
    Exact,
    Retained,
}

pub(super) fn validate_placement(
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
