//! Runner lease for `docs/spec/runner-protocol.md`.

use super::catalog::{RunnerToolEffectClass, tool_effect_class};
use super::credential_grant::CredentialDispatchAuthorization;
use super::enrollment::ValidatedRunnerRegistration;
use super::names::{RunnerDomainError, RunnerGeneration};
use crate::{
    ApprovedToolRequest, AuthorizedToolAttempt, EndedToolAttempt, RunnerId, RunnerLeaseId,
    SessionId, ToolAttemptDispatchCorrelation, ToolAttemptId, ToolBatch, ToolBatchExecutionFailure,
    ToolName,
};
use std::{sync::atomic::AtomicBool, sync::atomic::Ordering};

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
pub(super) struct ClaimedAttemptReplacementEvidence {
    pub(super) source: RunnerLeaseCorrelation,
    pub(super) replacement: ToolAttemptDispatchCorrelation,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum RunnerRetryAttemptEvidence {
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
    pub(super) authorized: AuthorizedToolAttempt,
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

    pub(super) fn into_parts(
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
    pub(super) correlation: RunnerLeaseCorrelation,
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
    pub(super) lease: RunnerLeaseId,
    pub(super) dispatch: ToolAttemptDispatchCorrelation,
    pub(super) runner: RunnerId,
    pub(super) tool: ToolName,
    pub(super) effect: RunnerToolEffectClass,
    pub(super) credential_authorization: Option<CredentialDispatchAuthorization>,
    pub(super) generation: RunnerGeneration,
    pub(super) state: RunnerLeaseState,
}

impl RunnerLease {
    pub(super) fn offer_validated(input: ValidatedRunnerLeaseOffer) -> Self {
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

pub(super) struct ValidatedRunnerLeaseOffer {
    pub(super) lease: RunnerLeaseId,
    pub(super) dispatch: ToolAttemptDispatchCorrelation,
    pub(super) runner: RunnerId,
    pub(super) tool: ToolName,
    pub(super) effect: RunnerToolEffectClass,
    pub(super) credential_authorization: Option<CredentialDispatchAuthorization>,
    pub(super) generation: RunnerGeneration,
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

    pub(super) fn into_retry_parts(self) -> Option<(RunnerLease, RunnerLeaseRetryAuthority)> {
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
    pub(super) source: RunnerLeaseRetrySource,
    pub(super) generation: RunnerGeneration,
    pub(super) claimed_attempt: Option<ToolAttemptId>,
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
pub(super) struct RunnerLeaseRetrySource {
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

    pub(super) fn matches(&self, lease: &RunnerLease) -> bool {
        self.correlation == lease.correlation()
            && self.effect == lease.effect
            && self.credential_authorization == lease.credential_authorization
            && self.state == lease.state
    }
}
