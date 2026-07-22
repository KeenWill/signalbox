//! Initial text-only model-call turn aggregate.
//!
//! ADR-0004, ADR-0005, ADR-0030, ADR-0035, ADR-0042, and ADR-0045 are
//! normative. This purpose-specific aggregate reconstitutes one active
//! accepted-input turn together with its current initial model call. It owns
//! target resolution against immutable configured definitions, the separate
//! prepared and send-authorization transitions, and the atomic terminal
//! candidates for the first text-only execution slice.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    AcceptedInputId, ActiveTurnPhase, AssistantText, ContextFrontierId, CurrentModelCall,
    CurrentModelCallState, CurrentTurnAttempt, CurrentTurnAttemptState, DirectModelSelection,
    EndedModelCall, EndedTurnAttempt, FrozenModelSelection, ModelCallDisposition, ModelCallId,
    ModelCallReconstitutionInput, NonEmptyIssuedOperationRefs, OriginConfiguration,
    PinnedProviderTarget, ReconstitutedModelCall, ResolvedContextFrontierSnapshot,
    ResolvedProviderTarget, SemanticTranscriptEntry, SemanticTranscriptEntryId,
    SemanticTranscriptEntryPayload, SessionId, TurnAttemptId, TurnDisposition, TurnId,
    UnstoppedAttemptDisposition, UserContent,
};

/// One immutable configured direct-selection to exact-target definition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelTargetDefinition {
    selection: DirectModelSelection,
    target: ResolvedProviderTarget,
}

impl ModelTargetDefinition {
    /// Associates one immutable direct-selection key with its exact target.
    pub const fn new(selection: DirectModelSelection, target: ResolvedProviderTarget) -> Self {
        Self { selection, target }
    }

    /// Returns the immutable selection key.
    pub const fn selection(&self) -> DirectModelSelection {
        self.selection
    }

    /// Returns the exact configured target.
    pub const fn target(&self) -> ResolvedProviderTarget {
        self.target
    }
}

/// Immutable domain projection of configured direct-selection targets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelTargetCatalog {
    targets: BTreeMap<DirectModelSelection, ResolvedProviderTarget>,
}

impl ModelTargetCatalog {
    /// Constructs a catalog, rejecting a repeated direct-selection key.
    pub fn try_from_definitions(
        definitions: impl IntoIterator<Item = ModelTargetDefinition>,
    ) -> Result<Self, ModelTargetCatalogError> {
        let mut targets = BTreeMap::new();
        for definition in definitions {
            if targets
                .insert(definition.selection, definition.target)
                .is_some()
            {
                return Err(ModelTargetCatalogError::DuplicateSelection {
                    selection: definition.selection,
                });
            }
        }
        Ok(Self { targets })
    }

    /// Resolves exactly the direct key frozen into a direct or alias request.
    pub fn resolve(
        &self,
        selection: FrozenModelSelection,
    ) -> Result<ResolvedModelSelection, ModelTargetResolutionError> {
        let direct = match selection {
            FrozenModelSelection::Direct(direct) => direct,
            FrozenModelSelection::FrozenAlias { definition, .. } => definition.selected(),
        };
        let Some(target) = self.targets.get(&direct).copied() else {
            return Err(ModelTargetResolutionError { selection, direct });
        };
        Ok(ResolvedModelSelection { selection, target })
    }
}

/// Why configured model targets could not form one catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelTargetCatalogError {
    /// The same immutable direct-selection key appeared twice.
    DuplicateSelection {
        /// The duplicated selection.
        selection: DirectModelSelection,
    },
}

/// A frozen selection whose exact target was resolved from the catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedModelSelection {
    selection: FrozenModelSelection,
    target: ResolvedProviderTarget,
}

impl ResolvedModelSelection {
    /// Returns the exact frozen requested selection.
    pub const fn selection(&self) -> FrozenModelSelection {
        self.selection
    }

    /// Returns the exact resolved target.
    pub const fn target(&self) -> ResolvedProviderTarget {
        self.target
    }
}

/// A frozen selection unavailable in the immutable configured catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelTargetResolutionError {
    selection: FrozenModelSelection,
    direct: DirectModelSelection,
}

/// Exact user content for one accepted-input origin referenced by a call
/// frontier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCallOriginContent {
    accepted_input: AcceptedInputId,
    content: UserContent,
}

impl ModelCallOriginContent {
    /// Associates accepted immutable user content with its accepted-input
    /// identity.
    pub const fn new(accepted_input: AcceptedInputId, content: UserContent) -> Self {
        Self {
            accepted_input,
            content,
        }
    }

    /// Returns the accepted input whose origin carries this content.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Borrows the exact user-authored scalar value.
    pub const fn content(&self) -> &UserContent {
        &self.content
    }
}

impl ModelTargetResolutionError {
    /// Returns the unresolved frozen selection.
    pub const fn selection(&self) -> FrozenModelSelection {
        self.selection
    }

    /// Returns the exact direct key whose target was unavailable.
    pub const fn direct_selection(&self) -> DirectModelSelection {
        self.direct
    }
}

/// Complete domain facts for reconstituting one live model-call execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCallExecutionReconstitutionInput {
    session: SessionId,
    turn: TurnId,
    configuration: OriginConfiguration,
    phase: ActiveTurnPhase,
    starting_snapshot: ResolvedContextFrontierSnapshot,
    frontier_entries: Vec<SemanticTranscriptEntry>,
    origin_contents: Vec<ModelCallOriginContent>,
    calls: Vec<ModelCallReconstitutionInput>,
}

impl ModelCallExecutionReconstitutionInput {
    /// Supplies the complete purpose-specific active-turn projection.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session: SessionId,
        turn: TurnId,
        configuration: OriginConfiguration,
        phase: ActiveTurnPhase,
        starting_snapshot: ResolvedContextFrontierSnapshot,
        frontier_entries: Vec<SemanticTranscriptEntry>,
        origin_contents: Vec<ModelCallOriginContent>,
        calls: Vec<ModelCallReconstitutionInput>,
    ) -> Self {
        Self {
            session,
            turn,
            configuration,
            phase,
            starting_snapshot,
            frontier_entries,
            origin_contents,
            calls,
        }
    }

    /// Reconstructs the canonical live aggregate without effects.
    pub fn reconstitute(self) -> Result<ModelCallExecution, ModelCallExecutionReconstitutionError> {
        reconstitute(self)
    }
}

/// Why live stored execution facts cannot reconstruct the initial aggregate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCallExecutionReconstitutionFailure {
    /// The supplied phase is a durable wait, not a live execution.
    TurnIsNotRunning,
    /// The starting snapshot belongs to a different session.
    StartingSnapshotSessionMismatch,
    /// The supplied frontier entries do not exactly back ordered membership.
    FrontierEntryMismatch,
    /// More than one model call was supplied to the initial-call slice.
    MultipleCalls,
    /// More than one user-content fact names the same accepted input.
    DuplicateOriginContent,
    /// A frontier origin has no exact accepted user content.
    MissingOriginContent,
    /// User content was supplied for an accepted input absent from the
    /// frontier.
    UnreferencedOriginContent,
    /// A call belongs to a different turn or session frontier.
    CallOwnershipMismatch,
    /// A call records a different frozen selection.
    CallSelectionMismatch,
    /// Stored call facts cannot reconstruct the accepted call lifecycle.
    InvalidCall,
    /// Attempt and call states do not form one accepted execution phase.
    LifecycleMismatch,
}

/// Reconstitution failure retaining the complete rejected input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCallExecutionReconstitutionError {
    input: Box<ModelCallExecutionReconstitutionInput>,
    failure: ModelCallExecutionReconstitutionFailure,
}

impl ModelCallExecutionReconstitutionError {
    /// Returns the exact failure classification.
    pub const fn failure(&self) -> ModelCallExecutionReconstitutionFailure {
        self.failure
    }

    /// Returns the complete rejected input.
    pub const fn input(&self) -> &ModelCallExecutionReconstitutionInput {
        &self.input
    }

    /// Returns the rejected input and failure.
    pub fn into_parts(
        self,
    ) -> (
        ModelCallExecutionReconstitutionInput,
        ModelCallExecutionReconstitutionFailure,
    ) {
        (*self.input, self.failure)
    }
}

/// One checked live initial model-call execution aggregate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCallExecution {
    session: SessionId,
    turn: TurnId,
    configuration: OriginConfiguration,
    current_attempt: CurrentTurnAttempt,
    starting_snapshot: ResolvedContextFrontierSnapshot,
    frontier_entries: Box<[SemanticTranscriptEntry]>,
    origin_contents: BTreeMap<AcceptedInputId, UserContent>,
    current_call: Option<CurrentModelCall>,
}

impl ModelCallExecution {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the owning logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Borrows the exact frozen origin configuration.
    pub const fn configuration(&self) -> &OriginConfiguration {
        &self.configuration
    }

    /// Borrows the current physical attempt.
    pub const fn current_attempt(&self) -> &CurrentTurnAttempt {
        &self.current_attempt
    }

    /// Borrows the current model call, when one has been checkpointed.
    pub const fn current_call(&self) -> Option<&CurrentModelCall> {
        self.current_call.as_ref()
    }

    /// Iterates over the exact ordered semantic frontier supplied to the call.
    pub fn frontier_entries(&self) -> impl ExactSizeIterator<Item = &SemanticTranscriptEntry> {
        self.frontier_entries.iter()
    }

    /// Borrows the exact user content for a frontier origin.
    pub fn origin_content(&self, accepted_input: AcceptedInputId) -> Option<&UserContent> {
        self.origin_contents.get(&accepted_input)
    }

    /// Creates the initial durable `Prepared` call checkpoint.
    pub fn prepare_initial_call(
        self,
        call: ModelCallId,
        resolution: ResolvedModelSelection,
    ) -> Result<PreparedInitialModelCall, ModelCallPreparationError> {
        let frozen = *self.configuration.effective().model();
        if resolution.selection != frozen {
            return Err(ModelCallPreparationError::new(
                self,
                ModelCallPreparationFailure::ResolutionSelectionMismatch,
            ));
        }
        if self.current_call.is_some() {
            return Err(ModelCallPreparationError::new(
                self,
                ModelCallPreparationFailure::CallAlreadyExists,
            ));
        }
        if self.current_attempt.state() != &CurrentTurnAttemptState::Prepared {
            return Err(ModelCallPreparationError::new(
                self,
                ModelCallPreparationFailure::AttemptIsNotPrepared,
            ));
        }
        let pinned = PinnedProviderTarget::pinned(self.turn, resolution.target);
        let prepared = CurrentModelCall::prepared(call, frozen, pinned, &self.starting_snapshot);
        Ok(PreparedInitialModelCall {
            session: self.session,
            turn: self.turn,
            attempt: self.current_attempt.id(),
            call: prepared,
        })
    }

    /// Returns a previously committed `Prepared` call for capability setup.
    pub fn resume_prepared_call(&self) -> Result<PreparedModelCallRequest, ModelCallResumeFailure> {
        if self.current_attempt.state() != &CurrentTurnAttemptState::Prepared {
            return Err(ModelCallResumeFailure::AttemptIsNotPrepared);
        }
        let Some(call) = &self.current_call else {
            return Err(ModelCallResumeFailure::CallMissing);
        };
        if call.state() != CurrentModelCallState::Prepared {
            return Err(ModelCallResumeFailure::CallIsNotPrepared);
        }
        Ok(PreparedModelCallRequest {
            session: self.session,
            turn: self.turn,
            attempt: self.current_attempt.id(),
            call: call.clone(),
            frontier_entries: self.frontier_entries.clone(),
            origin_contents: self.origin_contents.clone(),
        })
    }

    /// Atomically authorizes the prepared attempt and call to cross the send
    /// boundary.
    pub fn authorize_send(self) -> Result<AuthorizedModelCall, ModelCallAuthorizationError> {
        let fail = |execution, failure| ModelCallAuthorizationError {
            execution: Box::new(execution),
            failure,
        };
        let Some(call) = self.current_call.clone() else {
            return Err(fail(self, ModelCallAuthorizationFailure::CallMissing));
        };
        if call.state() != CurrentModelCallState::Prepared {
            return Err(fail(self, ModelCallAuthorizationFailure::CallIsNotPrepared));
        }
        let attempt = match self.current_attempt.clone().begin_running() {
            Ok(attempt) => attempt,
            Err(_) => {
                return Err(fail(
                    self,
                    ModelCallAuthorizationFailure::AttemptIsNotPrepared,
                ));
            }
        };
        let call = call.begin_in_flight().map_err(|_| {
            fail(
                self.clone(),
                ModelCallAuthorizationFailure::CallIsNotPrepared,
            )
        })?;
        Ok(AuthorizedModelCall {
            session: self.session,
            turn: self.turn,
            attempt,
            call,
            frontier_entries: self.frontier_entries,
            origin_contents: self.origin_contents,
        })
    }

    /// Closes target-resolution failure before a model call exists.
    pub fn fail_target_resolution(
        self,
        identities: FailedModelCallTurnIdentities,
    ) -> Result<FailedModelCallTurn, ModelCallClosureError> {
        if self.current_call.is_some() {
            return Err(ModelCallClosureError::CallStateMismatch);
        }
        close_failed_turn(
            self.session,
            self.turn,
            self.current_attempt,
            None,
            self.starting_snapshot,
            identities,
            UnstoppedAttemptDisposition::KnownFailure,
        )
    }

    /// Closes a trustworthy local capability-preparation failure before send.
    pub fn fail_prepared_call(
        self,
        identities: FailedModelCallTurnIdentities,
    ) -> Result<FailedModelCallTurn, ModelCallClosureError> {
        let Some(call) = self.current_call else {
            return Err(ModelCallClosureError::CallStateMismatch);
        };
        if call.state() != CurrentModelCallState::Prepared {
            return Err(ModelCallClosureError::CallStateMismatch);
        }
        let ended_call = call
            .end_classified(ModelCallDisposition::KnownFailed)
            .map_err(|_| ModelCallClosureError::CallStateMismatch)?;
        close_failed_turn(
            self.session,
            self.turn,
            self.current_attempt,
            Some(ended_call),
            self.starting_snapshot,
            identities,
            UnstoppedAttemptDisposition::KnownFailure,
        )
    }

    /// Applies ADR-0045's prior-process recovery rule for a committed model
    /// call after startup has established that no provider task survived.
    pub fn recover_after_restart(
        self,
        failure_identities: FailedModelCallTurnIdentities,
    ) -> Result<ModelCallTerminalOutcome, ModelCallClosureError> {
        let Some(call) = self.current_call else {
            return Err(ModelCallClosureError::CallStateMismatch);
        };
        match call.state() {
            CurrentModelCallState::Prepared => {
                let call = call
                    .end_classified(ModelCallDisposition::KnownFailed)
                    .map_err(|_| ModelCallClosureError::CallStateMismatch)?;
                close_failed_turn(
                    self.session,
                    self.turn,
                    self.current_attempt,
                    Some(call),
                    self.starting_snapshot,
                    failure_identities,
                    UnstoppedAttemptDisposition::Lost,
                )
                .map(ModelCallTerminalOutcome::Failed)
            }
            CurrentModelCallState::InFlight => {
                let call = call
                    .end_classified(ModelCallDisposition::Ambiguous)
                    .map_err(|_| ModelCallClosureError::CallStateMismatch)?;
                let call_id = call.id();
                let attempt = self
                    .current_attempt
                    .end_without_stop(UnstoppedAttemptDisposition::Lost)
                    .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
                let ambiguous_operations = NonEmptyIssuedOperationRefs::try_from_operations([
                    crate::IssuedOperationRef::ModelCall(call_id),
                ])
                .map_err(|_| ModelCallClosureError::AmbiguityConstructionFailed)?;
                Ok(ModelCallTerminalOutcome::AwaitingRecovery(
                    AmbiguousModelCallTurn {
                        session: self.session,
                        turn: self.turn,
                        call,
                        attempt,
                        ambiguous_operations,
                    },
                ))
            }
            CurrentModelCallState::CancellationRequested => {
                Err(ModelCallClosureError::CallStateMismatch)
            }
        }
    }
}

/// Why a fresh prepared checkpoint could not be derived.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCallPreparationFailure {
    /// The resolution belongs to a different frozen selection.
    ResolutionSelectionMismatch,
    /// The initial call has already been durably created.
    CallAlreadyExists,
    /// The current physical attempt is no longer prepared.
    AttemptIsNotPrepared,
}

/// Failed preparation retaining the unchanged live aggregate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCallPreparationError {
    execution: Box<ModelCallExecution>,
    failure: ModelCallPreparationFailure,
}

impl ModelCallPreparationError {
    fn new(execution: ModelCallExecution, failure: ModelCallPreparationFailure) -> Self {
        Self {
            execution: Box::new(execution),
            failure,
        }
    }

    /// Returns the failure classification.
    pub const fn failure(&self) -> ModelCallPreparationFailure {
        self.failure
    }

    /// Returns the unchanged live aggregate.
    pub const fn execution(&self) -> &ModelCallExecution {
        &self.execution
    }
}

/// A newly prepared call and the exact durable ownership facts to commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedInitialModelCall {
    session: SessionId,
    turn: TurnId,
    attempt: TurnAttemptId,
    call: CurrentModelCall,
}

impl PreparedInitialModelCall {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the owning turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the unchanged prepared attempt.
    pub const fn attempt(&self) -> TurnAttemptId {
        self.attempt
    }

    /// Borrows the new durable prepared call.
    pub const fn call(&self) -> &CurrentModelCall {
        &self.call
    }
}

/// Checked request material for a previously committed prepared call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedModelCallRequest {
    session: SessionId,
    turn: TurnId,
    attempt: TurnAttemptId,
    call: CurrentModelCall,
    frontier_entries: Box<[SemanticTranscriptEntry]>,
    origin_contents: BTreeMap<AcceptedInputId, UserContent>,
}

impl PreparedModelCallRequest {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the owning turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the exact prepared attempt.
    pub const fn attempt(&self) -> TurnAttemptId {
        self.attempt
    }

    /// Borrows the exact prepared call.
    pub const fn call(&self) -> &CurrentModelCall {
        &self.call
    }

    /// Iterates over the exact ordered semantic frontier.
    pub fn frontier_entries(&self) -> impl ExactSizeIterator<Item = &SemanticTranscriptEntry> {
        self.frontier_entries.iter()
    }

    /// Borrows the exact user content for a frontier origin.
    pub fn origin_content(&self, accepted_input: AcceptedInputId) -> Option<&UserContent> {
        self.origin_contents.get(&accepted_input)
    }
}

/// Why no prepared request can be resumed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCallResumeFailure {
    /// No call has been durably checkpointed.
    CallMissing,
    /// The call has already left `Prepared`.
    CallIsNotPrepared,
    /// The owning attempt has already left `Prepared`.
    AttemptIsNotPrepared,
}

/// Why send authorization could not be derived.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCallAuthorizationFailure {
    /// No call has been durably checkpointed.
    CallMissing,
    /// The call has already left `Prepared`.
    CallIsNotPrepared,
    /// The owning attempt has already left `Prepared`.
    AttemptIsNotPrepared,
}

/// Failed authorization retaining the unchanged aggregate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCallAuthorizationError {
    execution: Box<ModelCallExecution>,
    failure: ModelCallAuthorizationFailure,
}

impl ModelCallAuthorizationError {
    /// Returns the failure classification.
    pub const fn failure(&self) -> ModelCallAuthorizationFailure {
        self.failure
    }

    /// Returns the unchanged aggregate.
    pub const fn execution(&self) -> &ModelCallExecution {
        &self.execution
    }
}

/// Exact metadata authorized for one provider interaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedModelCall {
    session: SessionId,
    turn: TurnId,
    attempt: CurrentTurnAttempt,
    call: CurrentModelCall,
    frontier_entries: Box<[SemanticTranscriptEntry]>,
    origin_contents: BTreeMap<AcceptedInputId, UserContent>,
}

impl AuthorizedModelCall {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the owning turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Borrows the now-running attempt.
    pub const fn attempt(&self) -> &CurrentTurnAttempt {
        &self.attempt
    }

    /// Borrows the now-in-flight call.
    pub const fn call(&self) -> &CurrentModelCall {
        &self.call
    }

    /// Iterates over the exact ordered semantic frontier.
    pub fn frontier_entries(&self) -> impl ExactSizeIterator<Item = &SemanticTranscriptEntry> {
        self.frontier_entries.iter()
    }

    /// Borrows the exact user content for a frontier origin.
    pub fn origin_content(&self, accepted_input: AcceptedInputId) -> Option<&UserContent> {
        self.origin_contents.get(&accepted_input)
    }

    /// Applies one explicitly classified terminal provider observation.
    pub fn apply_terminal_observation(
        self,
        observation: ModelCallTerminalObservation,
        identities: ModelCallTerminalIdentities,
    ) -> Result<ModelCallTerminalOutcome, ModelCallClosureError> {
        let disposition = observation.disposition();
        let source_frontier = self.call.frontier();
        let ended_call = self
            .call
            .end_classified(disposition)
            .map_err(|_| ModelCallClosureError::CallStateMismatch)?;
        match observation {
            ModelCallTerminalObservation::Completed { assistant_text } => {
                let ModelCallTerminalIdentities::Completed(identities) = identities else {
                    return Err(ModelCallClosureError::IdentityShapeMismatch);
                };
                let ended_attempt = self
                    .attempt
                    .end_without_stop(UnstoppedAttemptDisposition::TurnCompleted)
                    .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
                let completed = complete_turn(
                    self.session,
                    self.turn,
                    ended_call,
                    ended_attempt,
                    self.frontier_entries.into_vec(),
                    assistant_text,
                    identities,
                )?;
                Ok(ModelCallTerminalOutcome::Completed(completed))
            }
            ModelCallTerminalObservation::KnownFailed | ModelCallTerminalObservation::Cancelled => {
                let ModelCallTerminalIdentities::Failed(identities) = identities else {
                    return Err(ModelCallClosureError::IdentityShapeMismatch);
                };
                let failed = close_failed_turn(
                    self.session,
                    self.turn,
                    self.attempt,
                    Some(ended_call),
                    ResolvedContextFrontierSnapshot::try_from_candidate(
                        self.session,
                        source_frontier.snapshot(),
                        self.frontier_entries
                            .iter()
                            .map(SemanticTranscriptEntry::reference)
                            .collect(),
                    )
                    .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?,
                    identities,
                    UnstoppedAttemptDisposition::KnownFailure,
                )?;
                Ok(ModelCallTerminalOutcome::Failed(failed))
            }
            ModelCallTerminalObservation::Refused => {
                let ModelCallTerminalIdentities::Refused(identities) = identities else {
                    return Err(ModelCallClosureError::IdentityShapeMismatch);
                };
                let ended_attempt = self
                    .attempt
                    .end_without_stop(UnstoppedAttemptDisposition::TurnRefused)
                    .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
                let source = ResolvedContextFrontierSnapshot::try_from_candidate(
                    self.session,
                    source_frontier.snapshot(),
                    self.frontier_entries
                        .iter()
                        .map(SemanticTranscriptEntry::reference)
                        .collect(),
                )
                .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
                let terminal_snapshot = source
                    .derive_appending_candidate(identities.terminal_frontier, Vec::new())
                    .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
                Ok(ModelCallTerminalOutcome::Refused(RefusedModelCallTurn {
                    session: self.session,
                    turn: self.turn,
                    call: ended_call,
                    attempt: ended_attempt,
                    disposition: TurnDisposition::Refused,
                    terminal_snapshot,
                }))
            }
            ModelCallTerminalObservation::Ambiguous => {
                if !matches!(identities, ModelCallTerminalIdentities::Ambiguous) {
                    return Err(ModelCallClosureError::IdentityShapeMismatch);
                }
                let call_id = ended_call.id();
                let ended_attempt = self
                    .attempt
                    .end_without_stop(UnstoppedAttemptDisposition::Ambiguous)
                    .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
                let ambiguous_operations = NonEmptyIssuedOperationRefs::try_from_operations([
                    crate::IssuedOperationRef::ModelCall(call_id),
                ])
                .map_err(|_| ModelCallClosureError::AmbiguityConstructionFailed)?;
                Ok(ModelCallTerminalOutcome::AwaitingRecovery(
                    AmbiguousModelCallTurn {
                        session: self.session,
                        turn: self.turn,
                        call: ended_call,
                        attempt: ended_attempt,
                        ambiguous_operations,
                    },
                ))
            }
        }
    }
}

/// One exact scripted or provider-adapter terminal classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallTerminalObservation {
    /// Definitive success with the complete ordered text-only response.
    Completed {
        /// Exact assistant text parts in final semantic order.
        assistant_text: Vec<AssistantText>,
    },
    /// Evidence establishes a known failure.
    KnownFailed,
    /// The authenticated complete exchange was explicitly refused.
    Refused,
    /// The physical provider interaction definitively cancelled.
    Cancelled,
    /// Provider acceptance or completion remains unresolved.
    Ambiguous,
}

impl ModelCallTerminalObservation {
    /// Returns the exact physical disposition declared by this observation.
    pub const fn disposition(&self) -> ModelCallDisposition {
        match self {
            Self::Completed { .. } => ModelCallDisposition::Completed,
            Self::KnownFailed => ModelCallDisposition::KnownFailed,
            Self::Refused => ModelCallDisposition::Refused,
            Self::Cancelled => ModelCallDisposition::Cancelled,
            Self::Ambiguous => ModelCallDisposition::Ambiguous,
        }
    }
}

/// Fresh identities for a successful text-only outcome transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletedModelCallIdentities {
    assistant_entries: Vec<SemanticTranscriptEntryId>,
    completion_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
}

impl CompletedModelCallIdentities {
    /// Supplies one identity per text part, the final marker, and frontier.
    pub fn new(
        assistant_entries: Vec<SemanticTranscriptEntryId>,
        completion_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            assistant_entries,
            completion_entry,
            terminal_frontier,
        }
    }
}

/// Fresh identities for a failed-turn outcome transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FailedModelCallTurnIdentities {
    failure_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
}

impl FailedModelCallTurnIdentities {
    /// Supplies the failure marker and terminal-frontier identities.
    pub const fn new(
        failure_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            failure_entry,
            terminal_frontier,
        }
    }
}

/// Fresh identity for a refusal terminal frontier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RefusedModelCallTurnIdentities {
    terminal_frontier: ContextFrontierId,
}

impl RefusedModelCallTurnIdentities {
    /// Supplies the new equal-content terminal frontier identity.
    pub const fn new(terminal_frontier: ContextFrontierId) -> Self {
        Self { terminal_frontier }
    }
}

/// Candidate identities matching one possible terminal observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallTerminalIdentities {
    /// Successful assistant-content and completion identities.
    Completed(CompletedModelCallIdentities),
    /// Known-failure or cause-free physical-cancellation identities.
    Failed(FailedModelCallTurnIdentities),
    /// Refusal terminal-frontier identity.
    Refused(RefusedModelCallTurnIdentities),
    /// Ambiguity creates no semantic entry or frontier.
    Ambiguous,
}

/// One terminal or durable-wait result from the observation transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallTerminalOutcome {
    /// Assistant content and turn completion committed atomically.
    Completed(CompletedModelCallTurn),
    /// The call and turn failed atomically.
    Failed(FailedModelCallTurn),
    /// The provider refusal terminalized the turn.
    Refused(RefusedModelCallTurn),
    /// Physical ambiguity ended the attempt and retained the slot.
    AwaitingRecovery(AmbiguousModelCallTurn),
}

/// One successful completed-turn commit candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletedModelCallTurn {
    session: SessionId,
    turn: TurnId,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    disposition: TurnDisposition,
    assistant_entries: Box<[SemanticTranscriptEntry]>,
    completion_entry: SemanticTranscriptEntry,
    terminal_snapshot: ResolvedContextFrontierSnapshot,
}

impl CompletedModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the completed turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the completed physical call.
    pub const fn call(&self) -> &EndedModelCall {
        &self.call
    }
    /// Borrows the ended attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }
    /// Borrows the completed turn disposition.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }
    /// Returns ordered assistant text entries.
    pub fn assistant_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.assistant_entries
    }
    /// Borrows the final completion marker.
    pub const fn completion_entry(&self) -> &SemanticTranscriptEntry {
        &self.completion_entry
    }
    /// Borrows the complete terminal frontier.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }
}

/// One failed-turn commit candidate, with an optional physical call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailedModelCallTurn {
    session: SessionId,
    turn: TurnId,
    call: Option<EndedModelCall>,
    attempt: EndedTurnAttempt,
    disposition: TurnDisposition,
    failure_entry: SemanticTranscriptEntry,
    terminal_snapshot: ResolvedContextFrontierSnapshot,
}

impl FailedModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the failed turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the physical call when one existed.
    pub const fn call(&self) -> Option<&EndedModelCall> {
        self.call.as_ref()
    }
    /// Borrows the ended attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }
    /// Borrows the failed turn disposition.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }
    /// Borrows the explicit failure marker.
    pub const fn failure_entry(&self) -> &SemanticTranscriptEntry {
        &self.failure_entry
    }
    /// Borrows the complete terminal frontier.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }
}

/// One refused-turn commit candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefusedModelCallTurn {
    session: SessionId,
    turn: TurnId,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    disposition: TurnDisposition,
    terminal_snapshot: ResolvedContextFrontierSnapshot,
}

impl RefusedModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the refused turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the refused physical call.
    pub const fn call(&self) -> &EndedModelCall {
        &self.call
    }
    /// Borrows the ended attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }
    /// Borrows the refused turn disposition.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }
    /// Borrows the terminal frontier.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }
}

/// One ambiguity wait candidate retaining immutable physical history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AmbiguousModelCallTurn {
    session: SessionId,
    turn: TurnId,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    ambiguous_operations: NonEmptyIssuedOperationRefs,
}

impl AmbiguousModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the active turn retaining the slot.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the ambiguous physical call.
    pub const fn call(&self) -> &EndedModelCall {
        &self.call
    }
    /// Borrows the ended attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }
    /// Borrows the exact recovery wait set.
    pub const fn ambiguous_operations(&self) -> &NonEmptyIssuedOperationRefs {
        &self.ambiguous_operations
    }
}

/// Why a guarded terminal candidate could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCallClosureError {
    /// Candidate identities do not match the observed disposition.
    IdentityShapeMismatch,
    /// The call cannot take the requested terminal transition.
    CallStateMismatch,
    /// The attempt cannot take the required terminal transition.
    AttemptStateMismatch,
    /// Assistant text and entry identity counts differ.
    AssistantIdentityCountMismatch,
    /// The exact terminal frontier could not preserve its source prefix.
    FrontierDerivationFailed,
    /// The exact nonempty ambiguity set could not be constructed.
    AmbiguityConstructionFailed,
}

fn reconstitute(
    input: ModelCallExecutionReconstitutionInput,
) -> Result<ModelCallExecution, ModelCallExecutionReconstitutionError> {
    let fail = |input, failure| ModelCallExecutionReconstitutionError {
        input: Box::new(input),
        failure,
    };
    let ActiveTurnPhase::Running { current_attempt } = &input.phase else {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::TurnIsNotRunning,
        ));
    };
    if input.starting_snapshot.frontier().owning_session() != input.session {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::StartingSnapshotSessionMismatch,
        ));
    }
    let entries = input
        .frontier_entries
        .iter()
        .map(|entry| (entry.reference(), entry))
        .collect::<BTreeMap<_, _>>();
    if entries.len() != input.frontier_entries.len()
        || input
            .starting_snapshot
            .ordered_entries()
            .any(|entry| !entries.contains_key(&entry))
        || input.starting_snapshot.entry_count() != entries.len()
    {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::FrontierEntryMismatch,
        ));
    }
    let mut origin_contents = BTreeMap::new();
    for origin in &input.origin_contents {
        if origin_contents
            .insert(origin.accepted_input, origin.content.clone())
            .is_some()
        {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::DuplicateOriginContent,
            ));
        }
    }
    let mut referenced_origins = BTreeSet::new();
    for entry in &input.frontier_entries {
        if let SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input } =
            entry.payload()
        {
            if !origin_contents.contains_key(accepted_input) {
                return Err(fail(
                    input,
                    ModelCallExecutionReconstitutionFailure::MissingOriginContent,
                ));
            }
            referenced_origins.insert(*accepted_input);
        }
    }
    if origin_contents
        .keys()
        .any(|accepted_input| !referenced_origins.contains(accepted_input))
    {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::UnreferencedOriginContent,
        ));
    }
    if input.calls.len() > 1 {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::MultipleCalls,
        ));
    }
    let current_call = if let Some(call) = input.calls.first() {
        if call.turn() != input.turn || call.frontier() != input.starting_snapshot.frontier() {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::CallOwnershipMismatch,
            ));
        }
        if call.selection() != *input.configuration.effective().model() {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::CallSelectionMismatch,
            ));
        }
        match call.reconstitute(&input.starting_snapshot) {
            Ok(ReconstitutedModelCall::Current(call)) => Some(call),
            Ok(ReconstitutedModelCall::Ended(_)) | Err(_) => {
                return Err(fail(
                    input,
                    ModelCallExecutionReconstitutionFailure::InvalidCall,
                ));
            }
        }
    } else {
        None
    };
    let lifecycle_valid = matches!(
        (
            current_attempt.state(),
            current_call.as_ref().map(CurrentModelCall::state)
        ),
        (CurrentTurnAttemptState::Prepared, None)
            | (
                CurrentTurnAttemptState::Prepared,
                Some(CurrentModelCallState::Prepared)
            )
            | (
                CurrentTurnAttemptState::Running,
                Some(CurrentModelCallState::InFlight)
            )
    );
    if !lifecycle_valid {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::LifecycleMismatch,
        ));
    }
    Ok(ModelCallExecution {
        session: input.session,
        turn: input.turn,
        configuration: input.configuration,
        current_attempt: current_attempt.clone(),
        starting_snapshot: input.starting_snapshot,
        frontier_entries: input.frontier_entries.into_boxed_slice(),
        origin_contents,
        current_call,
    })
}

fn complete_turn(
    session: SessionId,
    turn: TurnId,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    frontier_entries: Vec<SemanticTranscriptEntry>,
    assistant_text: Vec<AssistantText>,
    identities: CompletedModelCallIdentities,
) -> Result<CompletedModelCallTurn, ModelCallClosureError> {
    if assistant_text.len() != identities.assistant_entries.len() {
        return Err(ModelCallClosureError::AssistantIdentityCountMismatch);
    }
    let mut used = frontier_entries
        .iter()
        .map(SemanticTranscriptEntry::identity)
        .collect::<BTreeSet<_>>();
    if identities
        .assistant_entries
        .iter()
        .chain([&identities.completion_entry])
        .any(|identity| !used.insert(*identity))
    {
        return Err(ModelCallClosureError::FrontierDerivationFailed);
    }
    let assistant_entries = identities
        .assistant_entries
        .into_iter()
        .zip(assistant_text)
        .map(|(identity, value)| {
            SemanticTranscriptEntry::from_validated_parts(
                identity,
                session,
                SemanticTranscriptEntryPayload::AssistantText {
                    producing_call: call.id(),
                    value,
                },
            )
        })
        .collect::<Vec<_>>();
    let completion_entry = SemanticTranscriptEntry::from_validated_parts(
        identities.completion_entry,
        session,
        SemanticTranscriptEntryPayload::TurnCompleted { turn },
    );
    let source = ResolvedContextFrontierSnapshot::try_from_candidate(
        session,
        call.frontier().snapshot(),
        frontier_entries
            .iter()
            .map(SemanticTranscriptEntry::reference)
            .collect(),
    )
    .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    let appended = assistant_entries
        .iter()
        .map(SemanticTranscriptEntry::reference)
        .chain([completion_entry.reference()])
        .collect();
    let terminal_snapshot = source
        .derive_appending_candidate(identities.terminal_frontier, appended)
        .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    Ok(CompletedModelCallTurn {
        session,
        turn,
        call,
        attempt,
        disposition: TurnDisposition::Completed,
        assistant_entries: assistant_entries.into_boxed_slice(),
        completion_entry,
        terminal_snapshot,
    })
}

fn close_failed_turn(
    session: SessionId,
    turn: TurnId,
    attempt: CurrentTurnAttempt,
    call: Option<EndedModelCall>,
    source: ResolvedContextFrontierSnapshot,
    identities: FailedModelCallTurnIdentities,
    attempt_disposition: UnstoppedAttemptDisposition,
) -> Result<FailedModelCallTurn, ModelCallClosureError> {
    let ended_attempt = attempt
        .end_without_stop(attempt_disposition)
        .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
    let failure_entry = SemanticTranscriptEntry::from_validated_parts(
        identities.failure_entry,
        session,
        SemanticTranscriptEntryPayload::TurnFailed { turn },
    );
    let terminal_snapshot = source
        .derive_appending_candidate(
            identities.terminal_frontier,
            vec![failure_entry.reference()],
        )
        .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    Ok(FailedModelCallTurn {
        session,
        turn,
        call,
        attempt: ended_attempt,
        disposition: TurnDisposition::Failed,
        failure_entry,
        terminal_snapshot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AcceptedInputDisposition, AcceptedInputLifecycle, AcceptedInputQueueOrder,
        AcceptedInputSchedulingReconstitutionInput, AcceptedInputTurnActivationIdentities,
        AcceptedInputTurnSchedulingRecord, AcceptedInputTurnSchedulingRecordState, DeliveryRequest,
        ModelCallReconstitutionState, ModelSelectionOverride, ModelSelectionRequest,
        PerInputConfigurationChoices, Session, SessionConfigurationDefaults,
        SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
        SessionReconstitutionInput, TranscriptAncestry,
        test_support::{
            accepted_input_id, context_frontier_id, direct, model_call_id, provider_model_identity,
            semantic_transcript_entry_id, session_id, turn_attempt_id, turn_id,
        },
    };

    fn active_execution() -> ModelCallExecution {
        let session_id = session_id(1);
        let defaults = SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct(2)));
        let session = SessionReconstitutionInput::new(
            session_id,
            session_id,
            SessionCreationProvenance::new(
                SessionCreationCause::OwnerInitiated,
                TranscriptAncestry::None,
            ),
            session_id,
            SessionConfigurationDefaultsVersion::first(),
            session_id,
            SessionConfigurationDefaultsVersion::first(),
            defaults,
        )
        .reconstitute()
        .expect("session facts are correlated");
        execution_from_activation(session)
    }

    fn execution_from_activation(session: Session) -> ModelCallExecution {
        let checked = session
            .current_configuration_defaults()
            .derive_request(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            )
            .expect("defaults version is current");
        let configuration = OriginConfiguration::freeze(checked, |_| None)
            .expect("direct configuration needs no alias lookup");
        let turn = turn_id(3);
        let record = AcceptedInputTurnSchedulingRecord::new(
            session.id(),
            turn,
            session.id(),
            AcceptedInputLifecycle::new(
                accepted_input_id(4),
                AcceptedInputDisposition::OriginOf(turn),
            ),
            session.id(),
            turn,
            AcceptedInputQueueOrder::ordinary(crate::SessionInputPosition::first()),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            configuration.clone(),
            AcceptedInputTurnSchedulingRecordState::Queued,
        );
        let activation = AcceptedInputSchedulingReconstitutionInput::new(
            session,
            vec![record],
            Vec::new(),
            Vec::new(),
            None,
        )
        .reconstitute()
        .expect("queued scheduling projection is complete")
        .prepare_earliest_queued_activation(AcceptedInputTurnActivationIdentities::new(
            semantic_transcript_entry_id(5),
            context_frontier_id(6),
            turn_attempt_id(7),
        ))
        .expect("first turn is eligible");
        let (turn, origin, snapshot) = activation.into_parts();
        ModelCallExecutionReconstitutionInput::new(
            turn.session(),
            turn.turn(),
            turn.configuration().clone(),
            turn.phase().clone(),
            snapshot,
            vec![origin],
            vec![ModelCallOriginContent::new(
                accepted_input_id(4),
                UserContent::try_text(String::from("hello")).expect("test content is valid"),
            )],
            Vec::new(),
        )
        .reconstitute()
        .expect("activation facts reconstruct live execution")
    }

    fn resolution() -> ResolvedModelSelection {
        ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
            direct(2),
            ResolvedProviderTarget::naming(provider_model_identity(8)),
        )])
        .expect("one definition is unique")
        .resolve(FrozenModelSelection::Direct(direct(2)))
        .expect("configured selection resolves")
    }

    fn prepared_execution() -> ModelCallExecution {
        let initial = active_execution();
        let prepared = initial
            .clone()
            .prepare_initial_call(model_call_id(9), resolution())
            .expect("initial prepared checkpoint is valid");
        ModelCallExecutionReconstitutionInput::new(
            initial.session,
            initial.turn,
            initial.configuration,
            ActiveTurnPhase::Running {
                current_attempt: initial.current_attempt,
            },
            initial.starting_snapshot.clone(),
            initial.frontier_entries.into_vec(),
            initial
                .origin_contents
                .into_iter()
                .map(|(accepted_input, content)| {
                    ModelCallOriginContent::new(accepted_input, content)
                })
                .collect(),
            vec![ModelCallReconstitutionInput::new(
                prepared.call().id(),
                prepared.call().turn(),
                prepared.call().selection(),
                prepared.call().target(),
                prepared.call().frontier(),
                ModelCallReconstitutionState::Prepared,
            )],
        )
        .reconstitute()
        .expect("prepared facts reconstruct")
    }

    fn in_flight_execution() -> ModelCallExecution {
        let prepared = prepared_execution();
        let authorized = prepared
            .clone()
            .authorize_send()
            .expect("prepared execution may authorize send");
        ModelCallExecutionReconstitutionInput::new(
            prepared.session,
            prepared.turn,
            prepared.configuration,
            ActiveTurnPhase::Running {
                current_attempt: authorized.attempt,
            },
            prepared.starting_snapshot,
            prepared.frontier_entries.into_vec(),
            prepared
                .origin_contents
                .into_iter()
                .map(|(accepted_input, content)| {
                    ModelCallOriginContent::new(accepted_input, content)
                })
                .collect(),
            vec![ModelCallReconstitutionInput::new(
                authorized.call.id(),
                authorized.call.turn(),
                authorized.call.selection(),
                authorized.call.target(),
                authorized.call.frontier(),
                ModelCallReconstitutionState::InFlight,
            )],
        )
        .reconstitute()
        .expect("in-flight facts reconstruct")
    }

    /// S02 / INV-014 / INV-015: target resolution records the frozen
    /// selection, target, and exact frontier before send authorization.
    #[test]
    fn s02_inv014_inv015_preparation_is_a_distinct_checkpoint() {
        let execution = active_execution();
        let prepared = execution
            .prepare_initial_call(model_call_id(9), resolution())
            .expect("initial call may be prepared");

        assert_eq!(prepared.call().state(), CurrentModelCallState::Prepared);
        assert_eq!(
            prepared.call().selection(),
            FrozenModelSelection::Direct(direct(2))
        );
        assert_eq!(
            prepared.call().target().identity(),
            provider_model_identity(8)
        );
        assert_eq!(
            prepared.call().frontier().snapshot(),
            context_frontier_id(6)
        );
    }

    /// S02 / INV-005: provider rendering receives the frontier in semantic
    /// order and the exact accepted user content keyed by its origin identity.
    #[test]
    fn s02_inv005_prepared_request_preserves_exact_origin_content() {
        let execution = prepared_execution();
        let request = execution
            .resume_prepared_call()
            .expect("a committed prepared call yields rendering material");
        let entry = request
            .frontier_entries()
            .next()
            .expect("the first-turn frontier contains its origin");

        assert!(matches!(
            entry.payload(),
            SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
                if *accepted_input == accepted_input_id(4)
        ));
        assert_eq!(
            request
                .origin_content(accepted_input_id(4))
                .expect("the checked origin has exact user content")
                .text()
                .as_str(),
            "hello"
        );
    }

    /// S02 / INV-006 / INV-009: authorization advances the exact attempt and
    /// call together without changing identity or frontier.
    #[test]
    fn s02_inv006_inv009_authorization_advances_attempt_and_call_together() {
        let authorized = prepared_execution()
            .authorize_send()
            .expect("prepared execution may authorize send");

        assert_eq!(
            authorized.attempt().state(),
            &CurrentTurnAttemptState::Running
        );
        assert_eq!(authorized.call().state(), CurrentModelCallState::InFlight);
        assert_eq!(authorized.call().id(), model_call_id(9));
        assert_eq!(
            authorized.call().frontier().snapshot(),
            context_frontier_id(6)
        );
    }

    /// S02 / INV-005 / INV-006 / INV-032: successful final text, physical
    /// completion, attempt/turn completion, and the final marker share one
    /// prefix-preserving candidate.
    #[test]
    fn s02_inv005_inv006_inv032_completion_is_atomic_and_ordered() {
        let authorized = prepared_execution()
            .authorize_send()
            .expect("prepared execution may authorize send");
        let outcome = authorized
            .apply_terminal_observation(
                ModelCallTerminalObservation::Completed {
                    assistant_text: vec![
                        AssistantText::try_new("first".to_string()).expect("nonempty text"),
                        AssistantText::try_new(" second ".to_string()).expect("nonempty text"),
                    ],
                },
                ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                    vec![
                        semantic_transcript_entry_id(10),
                        semantic_transcript_entry_id(11),
                    ],
                    semantic_transcript_entry_id(12),
                    context_frontier_id(13),
                )),
            )
            .expect("definitive text completion is admissible");
        let ModelCallTerminalOutcome::Completed(completed) = outcome else {
            panic!("completed evidence selects completed outcome");
        };

        assert_eq!(
            completed.call().disposition(),
            ModelCallDisposition::Completed
        );
        assert_eq!(completed.disposition(), &TurnDisposition::Completed);
        assert_eq!(completed.assistant_entries().len(), 2);
        assert!(matches!(
            completed.completion_entry().payload(),
            SemanticTranscriptEntryPayload::TurnCompleted { turn } if *turn == turn_id(3)
        ));
        assert_eq!(
            completed
                .terminal_snapshot()
                .ordered_entries()
                .collect::<Vec<_>>(),
            vec![
                crate::SemanticTranscriptEntryRef::from_source(
                    session_id(1),
                    semantic_transcript_entry_id(5)
                ),
                crate::SemanticTranscriptEntryRef::from_source(
                    session_id(1),
                    semantic_transcript_entry_id(10)
                ),
                crate::SemanticTranscriptEntryRef::from_source(
                    session_id(1),
                    semantic_transcript_entry_id(11)
                ),
                crate::SemanticTranscriptEntryRef::from_source(
                    session_id(1),
                    semantic_transcript_entry_id(12)
                ),
            ]
        );
    }

    /// S04 / INV-025 / INV-026: ambiguous physical completion ends the live
    /// attempt and retains the exact call in a durable recovery wait.
    #[test]
    fn s04_inv025_inv026_ambiguity_preserves_call_and_waits() {
        let authorized = prepared_execution()
            .authorize_send()
            .expect("prepared execution may authorize send");
        let outcome = authorized
            .apply_terminal_observation(
                ModelCallTerminalObservation::Ambiguous,
                ModelCallTerminalIdentities::Ambiguous,
            )
            .expect("ambiguous evidence is representable");
        let ModelCallTerminalOutcome::AwaitingRecovery(waiting) = outcome else {
            panic!("ambiguous evidence selects recovery wait");
        };

        assert_eq!(
            waiting.call().disposition(),
            ModelCallDisposition::Ambiguous
        );
        assert!(
            waiting
                .ambiguous_operations()
                .contains(crate::IssuedOperationRef::ModelCall(model_call_id(9)))
        );
    }

    /// S04 / INV-034: startup converts an unsent prepared call to known
    /// failure while recording that the prior-process attempt was lost.
    #[test]
    fn s04_inv034_restart_closes_prepared_call_as_known_failed_and_attempt_lost() {
        let outcome = prepared_execution()
            .recover_after_restart(FailedModelCallTurnIdentities::new(
                semantic_transcript_entry_id(10),
                context_frontier_id(11),
            ))
            .expect("startup may close an unsent prepared call");
        let ModelCallTerminalOutcome::Failed(failed) = outcome else {
            panic!("a prior-process prepared call selects failed outcome");
        };

        assert_eq!(
            failed
                .call()
                .expect("the prepared call becomes terminal")
                .disposition(),
            ModelCallDisposition::KnownFailed
        );
        assert!(matches!(
            failed.attempt().end(),
            crate::AttemptEnd::WithoutStop {
                disposition: UnstoppedAttemptDisposition::Lost,
            }
        ));
    }

    /// S04 / INV-025 / INV-026 / INV-034: startup cannot infer the fate of an
    /// issued prior-process call, so it records ambiguity and a lost attempt.
    #[test]
    fn s04_inv025_inv026_inv034_restart_preserves_in_flight_call_as_ambiguous() {
        let outcome = in_flight_execution()
            .recover_after_restart(FailedModelCallTurnIdentities::new(
                semantic_transcript_entry_id(10),
                context_frontier_id(11),
            ))
            .expect("startup may classify an abandoned issued call");
        let ModelCallTerminalOutcome::AwaitingRecovery(waiting) = outcome else {
            panic!("a prior-process issued call selects recovery wait");
        };

        assert_eq!(
            waiting.call().disposition(),
            ModelCallDisposition::Ambiguous
        );
        assert!(matches!(
            waiting.attempt().end(),
            crate::AttemptEnd::WithoutStop {
                disposition: UnstoppedAttemptDisposition::Lost,
            }
        ));
        assert!(
            waiting
                .ambiguous_operations()
                .contains(crate::IssuedOperationRef::ModelCall(model_call_id(9)))
        );
    }

    /// S02 / INV-006: definitive known failure closes the physical call and
    /// logical turn as failed in one candidate.
    #[test]
    fn s02_inv006_known_failure_closes_call_attempt_and_turn() {
        let outcome = prepared_execution()
            .authorize_send()
            .expect("prepared execution may authorize send")
            .apply_terminal_observation(
                ModelCallTerminalObservation::KnownFailed,
                ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                    semantic_transcript_entry_id(10),
                    context_frontier_id(11),
                )),
            )
            .expect("known-failure evidence is admissible");
        let ModelCallTerminalOutcome::Failed(failed) = outcome else {
            panic!("known-failure evidence selects failed outcome");
        };

        assert_eq!(
            failed
                .call()
                .expect("the issued call is terminal")
                .disposition(),
            ModelCallDisposition::KnownFailed
        );
        assert_eq!(failed.disposition(), &TurnDisposition::Failed);
    }

    /// S02 / INV-006: a cause-free physical cancellation is not a logical
    /// cancellation and closes the logical turn as failed.
    #[test]
    fn s02_inv006_cause_free_physical_cancellation_fails_turn() {
        let outcome = prepared_execution()
            .authorize_send()
            .expect("prepared execution may authorize send")
            .apply_terminal_observation(
                ModelCallTerminalObservation::Cancelled,
                ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                    semantic_transcript_entry_id(10),
                    context_frontier_id(11),
                )),
            )
            .expect("cause-free physical cancellation is admissible");
        let ModelCallTerminalOutcome::Failed(failed) = outcome else {
            panic!("cause-free cancellation selects failed outcome");
        };

        assert_eq!(
            failed
                .call()
                .expect("the issued call is terminal")
                .disposition(),
            ModelCallDisposition::Cancelled
        );
        assert_eq!(failed.disposition(), &TurnDisposition::Failed);
    }

    /// S02 / INV-006: an explicit provider refusal preserves its physical and
    /// logical classifications without manufacturing semantic response text.
    #[test]
    fn s02_inv006_refusal_closes_call_attempt_and_turn_without_content() {
        let outcome = prepared_execution()
            .authorize_send()
            .expect("prepared execution may authorize send")
            .apply_terminal_observation(
                ModelCallTerminalObservation::Refused,
                ModelCallTerminalIdentities::Refused(RefusedModelCallTurnIdentities::new(
                    context_frontier_id(11),
                )),
            )
            .expect("explicit refusal evidence is admissible");
        let ModelCallTerminalOutcome::Refused(refused) = outcome else {
            panic!("refusal evidence selects refused outcome");
        };

        assert_eq!(refused.call().disposition(), ModelCallDisposition::Refused);
        assert_eq!(refused.disposition(), &TurnDisposition::Refused);
        assert_eq!(refused.terminal_snapshot().entry_count(), 1);
    }
}
