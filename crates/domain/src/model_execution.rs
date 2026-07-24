//! Model-call turn aggregate with intra-turn tool yields.
//!
//! docs/spec/turn-lifecycle-and-scheduling.md,
//! docs/spec/model-call-execution.md, docs/spec/sessions-and-transcript.md,
//! and docs/spec/persistence-protocol.md are normative. This purpose-specific
//! aggregate reconstitutes one active accepted-input turn together with its
//! current model call. It owns target resolution against immutable
//! configured definitions, the separate prepared and send-authorization
//! transitions, atomic terminal candidates, and the response-side transition
//! that yields a tool-request batch without terminalizing the turn.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputLifecycle, AcceptedInputQueueOrder,
    AcceptedInputTurnStart, ActivatedAcceptedInputTurn, ActiveTurnPhase,
    AppliedInterruptCommandResult, AppliedInterruptProof, AssistantResponsePart, AssistantText,
    AttemptEnd, AwaitingToolRecovery, CancellationStopDisposition, ContextFrontierId,
    CurrentModelCall, CurrentModelCallState, CurrentTurnAttempt, CurrentTurnAttemptState,
    DangerousToolAutoApproval, DirectModelSelection, EffectiveConfiguration, EndedModelCall,
    EndedToolAttempt, EndedTurnAttempt, FrozenModelSelection, InitialToolApproval,
    ModelCallDisposition, ModelCallId, ModelCallReconstitutionInput, NonEmptyIssuedOperationRefs,
    OriginConfiguration, PinnedProviderTarget, PinnedProviderTargetReconstitutionInput,
    PreparedToolResultProjection, ReconciliationMarker, ReconstitutedModelCall,
    ReconstitutedSubmitInput, ResolvedContextFrontierReconstitutionInput,
    ResolvedContextFrontierSnapshot, ResolvedProviderTarget, SemanticTranscriptEntry,
    SemanticTranscriptEntryId, SemanticTranscriptEntryPayload, SessionId, SteeringBinding,
    SteeringReclassificationReason, SubmitInputResult, SubmitInputTurnOriginReconstitutionInput,
    ToolApprovalDecision, ToolApprovalResolution, ToolRequest, ToolRequestId, ToolRequestOrdinal,
    ToolUsingAssistantResponse, TurnAttemptId, TurnAttemptStopCauses, TurnDisposition, TurnId,
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
    #[cfg(test)]
    pub(crate) const fn from_validated_parts(
        accepted_input: AcceptedInputId,
        content: UserContent,
    ) -> Self {
        Self {
            accepted_input,
            content,
        }
    }

    /// Derives exact user content from one checked applied input receipt.
    pub fn from_recorded_submit(recorded: &ReconstitutedSubmitInput) -> Option<Self> {
        let SubmitInputResult::Applied(applied) = recorded.result() else {
            return None;
        };
        (applied.session() == recorded.command().session()).then(|| Self {
            accepted_input: applied.accepted_input(),
            content: recorded.command().content().clone(),
        })
    }

    /// Derives exact origin content from a fully validated direct or
    /// reclassified accepted-input turn-origin chain.
    pub fn from_reconstituted_turn_origin(
        origin: &SubmitInputTurnOriginReconstitutionInput,
    ) -> Option<Self> {
        let (accepted_input, content) = origin.validated_origin_content()?;
        Some(Self {
            accepted_input,
            content,
        })
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
    active_turn: ActivatedAcceptedInputTurn,
    targets: ModelTargetCatalog,
    starting_snapshot: ResolvedContextFrontierSnapshot,
    continuation_snapshot: Option<ResolvedContextFrontierReconstitutionInput>,
    call_snapshot: Option<ResolvedContextFrontierReconstitutionInput>,
    frontier_entries: Vec<SemanticTranscriptEntry>,
    origin_contents: Vec<ModelCallOriginContent>,
    pinned_target: Option<PinnedProviderTargetReconstitutionInput>,
    calls: Vec<ModelCallReconstitutionInput>,
    tool_result_correlations: Vec<ToolResultAttemptCorrelation>,
    tool_denial_correlations: Vec<ToolApprovalResolution>,
    uncommitted_tool_result_projection: Option<PreparedToolResultProjection>,
}

impl ModelCallExecutionReconstitutionInput {
    /// Supplies the complete purpose-specific active-turn projection.
    pub fn new(
        active_turn: ActivatedAcceptedInputTurn,
        targets: ModelTargetCatalog,
        starting_snapshot: ResolvedContextFrontierSnapshot,
        frontier_entries: Vec<SemanticTranscriptEntry>,
        origin_contents: Vec<ModelCallOriginContent>,
        pinned_target: Option<PinnedProviderTargetReconstitutionInput>,
        calls: Vec<ModelCallReconstitutionInput>,
    ) -> Self {
        Self {
            active_turn,
            targets,
            starting_snapshot,
            continuation_snapshot: None,
            call_snapshot: None,
            frontier_entries,
            origin_contents,
            pinned_target,
            calls,
            tool_result_correlations: Vec::new(),
            tool_denial_correlations: Vec::new(),
            uncommitted_tool_result_projection: None,
        }
    }

    /// Supplies exact request and producing-call ownership for every physical
    /// tool attempt referenced by the current frontier.
    pub fn with_tool_result_correlations(
        mut self,
        correlations: Vec<ToolResultAttemptCorrelation>,
    ) -> Self {
        self.tool_result_correlations = correlations;
        self
    }

    /// Supplies the exact durable denial resolution for every denied request
    /// referenced by the current frontier.
    pub fn with_tool_denial_correlations(
        mut self,
        correlations: Vec<ToolApprovalResolution>,
    ) -> Self {
        self.tool_denial_correlations = correlations;
        self
    }

    /// Supplies the domain-prepared result projection being consumed inside
    /// the same transaction that will insert its continuation call.
    ///
    /// A durably visible resolved frontier without a prepared call is rejected;
    /// this proof admits only the transaction-local intermediate shape.
    pub fn with_uncommitted_tool_result_projection(
        mut self,
        projection: PreparedToolResultProjection,
    ) -> Self {
        self.uncommitted_tool_result_projection = Some(projection);
        self
    }

    /// Supplies the non-starting snapshot named by a steering-consuming call.
    pub fn with_call_snapshot(
        mut self,
        call_snapshot: ResolvedContextFrontierReconstitutionInput,
    ) -> Self {
        self.call_snapshot = Some(call_snapshot);
        self
    }

    /// Supplies the all-resolved tool-result frontier that precedes a fresh
    /// continuation call.
    ///
    /// The owning persistence aggregate remains responsible for proving the
    /// tool-batch/result correlation. This seam validates complete snapshot
    /// shape, turn ownership, and preservation of the eligibility-fixed
    /// starting prefix.
    pub fn with_continuation_snapshot(
        mut self,
        continuation_snapshot: ResolvedContextFrontierReconstitutionInput,
    ) -> Self {
        self.continuation_snapshot = Some(continuation_snapshot);
        self
    }

    /// Reconstructs the canonical live aggregate without effects.
    pub fn reconstitute(self) -> Result<ModelCallExecution, ModelCallExecutionReconstitutionError> {
        reconstitute(self)
    }
}

/// Stored ownership facts for one tool attempt referenced by model input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolResultAttemptCorrelation {
    attempt: crate::ToolAttemptId,
    request: crate::ToolRequestId,
    producing_call: ModelCallId,
}

impl ToolResultAttemptCorrelation {
    /// Captures one exact attempt-to-request-to-producing-call relationship.
    pub const fn new(
        attempt: crate::ToolAttemptId,
        request: crate::ToolRequestId,
        producing_call: ModelCallId,
    ) -> Self {
        Self {
            attempt,
            request,
            producing_call,
        }
    }

    /// Returns the physical attempt.
    pub const fn attempt(&self) -> crate::ToolAttemptId {
        self.attempt
    }

    /// Returns the logical request executed by the attempt.
    pub const fn request(&self) -> crate::ToolRequestId {
        self.request
    }

    /// Returns the model call that proposed the request.
    pub const fn producing_call(&self) -> ModelCallId {
        self.producing_call
    }
}

/// Why live stored execution facts cannot reconstruct the initial aggregate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCallExecutionReconstitutionFailure {
    /// The supplied phase is a durable wait, not a live execution.
    TurnIsNotRunning,
    /// The starting snapshot belongs to a different session.
    StartingSnapshotSessionMismatch,
    /// The supplied snapshot is not the turn's eligibility-fixed start.
    StartingSnapshotMismatch,
    /// A non-starting call frontier was omitted.
    CallSnapshotMissing,
    /// A continuation snapshot was supplied outside a fresh continuation.
    ContinuationSnapshotUnexpected,
    /// A continuation snapshot is malformed or does not preserve the start.
    ContinuationSnapshotMismatch,
    /// A call snapshot was supplied without a consuming call or steering.
    CallSnapshotUnexpected,
    /// The supplied call snapshot is not the call's exact prefix extension.
    CallSnapshotMismatch,
    /// The supplied frontier entries do not exactly back ordered membership.
    FrontierEntryMismatch,
    /// Tool-result attempts do not exactly belong to the referenced requests
    /// and producing model calls.
    ToolResultCorrelationMismatch,
    /// Tool-denial entries do not exactly match durable denied resolutions.
    ToolDenialCorrelationMismatch,
    /// More than one model call was supplied to the initial-call slice.
    MultipleCalls,
    /// More than one user-content fact names the same accepted input.
    DuplicateOriginContent,
    /// A frontier origin has no exact accepted user content.
    MissingOriginContent,
    /// User content was supplied for an accepted input absent from the
    /// frontier and pending steering inventory.
    UnreferencedOriginContent,
    /// Consumed steering does not exactly match the call frontier suffix.
    ConsumedSteeringMismatch,
    /// A call belongs to a different turn or session frontier.
    CallOwnershipMismatch,
    /// A call records a different frozen selection.
    CallSelectionMismatch,
    /// A stored call target contradicts an available immutable catalog entry.
    CallTargetMismatch,
    /// A call exists without the independently stored turn-pinned target.
    PinnedTargetMissing,
    /// A pinned target exists even though no call was atomically created.
    PinnedTargetUnexpected,
    /// The stored pinned target belongs to another turn.
    PinnedTargetTurnMismatch,
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
    active_turn: ActivatedAcceptedInputTurn,
    session: SessionId,
    turn: TurnId,
    configuration: OriginConfiguration,
    start: AcceptedInputTurnStart,
    targets: ModelTargetCatalog,
    current_attempt: CurrentTurnAttempt,
    starting_snapshot: ResolvedContextFrontierSnapshot,
    current_snapshot: ResolvedContextFrontierSnapshot,
    frontier_entries: Box<[SemanticTranscriptEntry]>,
    origin_contents: BTreeMap<AcceptedInputId, UserContent>,
    pinned_target: Option<PinnedProviderTarget>,
    current_call: Option<CurrentModelCall>,
    tool_continuation_frontier: bool,
}

impl ModelCallExecution {
    /// Borrows the checked active-turn facts that establish ownership.
    pub const fn active_turn(&self) -> &ActivatedAcceptedInputTurn {
        &self.active_turn
    }

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

    /// Returns the exact eligibility-fixed lineage and starting frontier.
    pub const fn start(&self) -> AcceptedInputTurnStart {
        self.start
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
    ) -> Result<PreparedInitialModelCall, ModelCallPreparationError> {
        self.prepare_initial_call_consuming_steering(call, Vec::new(), None)
    }

    /// Creates the initial durable call while consuming the complete pending
    /// steering inventory.
    pub fn prepare_initial_call_consuming_steering(
        self,
        call: ModelCallId,
        steering_entries: Vec<SemanticTranscriptEntryId>,
        steering_frontier: Option<ContextFrontierId>,
    ) -> Result<PreparedInitialModelCall, ModelCallPreparationError> {
        let frozen = *self.configuration.effective().model();
        if self.current_call.is_some() {
            return Err(ModelCallPreparationError::new(
                self,
                ModelCallPreparationFailure::CallAlreadyExists,
            ));
        }
        if !self.attempt_accepts_prepared_call() {
            return Err(ModelCallPreparationError::new(
                self,
                ModelCallPreparationFailure::AttemptIsNotPrepared,
            ));
        }
        let pending = self.active_turn.pending_steering().to_vec();
        if pending.len() != steering_entries.len() {
            return Err(ModelCallPreparationError::new(
                self,
                ModelCallPreparationFailure::SteeringIdentityCountMismatch,
            ));
        }
        if pending.is_empty() != steering_frontier.is_none() {
            return Err(ModelCallPreparationError::new(
                self,
                ModelCallPreparationFailure::SteeringFrontierIdentityMismatch,
            ));
        }
        let pinned = if let Some(pinned) = self.pinned_target {
            pinned
        } else {
            let resolution = match self.targets.resolve(frozen) {
                Ok(resolution) => resolution,
                Err(error) => {
                    return Err(ModelCallPreparationError::target_unavailable(self, error));
                }
            };
            PinnedProviderTarget::pinned(self.turn, resolution.target)
        };
        let mut distinct_entries = self
            .frontier_entries
            .iter()
            .map(SemanticTranscriptEntry::identity)
            .collect::<BTreeSet<_>>();
        let mut consumed_steering = Vec::with_capacity(pending.len());
        let mut semantic_entries = Vec::with_capacity(pending.len());
        for (pending, entry) in pending.iter().zip(steering_entries) {
            let AcceptedInputDisposition::PendingSteering { binding } =
                pending.lifecycle().disposition()
            else {
                return Err(ModelCallPreparationError::new(
                    self,
                    ModelCallPreparationFailure::SteeringCorrelationMismatch,
                ));
            };
            if binding.source_turn() != self.turn
                || !distinct_entries.insert(entry)
                || !self.origin_contents.contains_key(&pending.accepted_input())
            {
                return Err(ModelCallPreparationError::new(
                    self,
                    ModelCallPreparationFailure::SteeringCorrelationMismatch,
                ));
            }
            let lifecycle = pending
                .lifecycle()
                .clone()
                .consume_as_steering(call)
                .map_err(|_| {
                    ModelCallPreparationError::new(
                        self.clone(),
                        ModelCallPreparationFailure::SteeringCorrelationMismatch,
                    )
                })?;
            let semantic_entry = SemanticTranscriptEntry::from_validated_parts(
                entry,
                self.session,
                SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                    accepted_input: pending.accepted_input(),
                    source_turn: self.turn,
                },
            );
            consumed_steering.push(PreparedSteeringConsumption {
                accepted_input: lifecycle,
                semantic_entry: semantic_entry.clone(),
            });
            semantic_entries.push(semantic_entry);
        }
        let call_snapshot = if semantic_entries.is_empty() {
            self.current_snapshot.clone()
        } else {
            self.current_snapshot
                .derive_appending_candidate(
                    match steering_frontier {
                        Some(frontier) => frontier,
                        None => {
                            return Err(ModelCallPreparationError::new(
                                self,
                                ModelCallPreparationFailure::SteeringFrontierIdentityMismatch,
                            ));
                        }
                    },
                    semantic_entries
                        .iter()
                        .map(SemanticTranscriptEntry::reference)
                        .collect(),
                )
                .map_err(|_| {
                    ModelCallPreparationError::new(
                        self.clone(),
                        ModelCallPreparationFailure::SteeringFrontierIdentityMismatch,
                    )
                })?
        };
        let prepared = CurrentModelCall::prepared(
            call,
            self.current_attempt.id(),
            frozen,
            pinned,
            &call_snapshot,
        );
        Ok(PreparedInitialModelCall {
            session: self.session,
            turn: self.turn,
            attempt: self.current_attempt.id(),
            call: prepared,
            consumed_steering: consumed_steering.into_boxed_slice(),
            steering_snapshot: (!semantic_entries.is_empty()).then_some(call_snapshot),
        })
    }

    /// Returns a previously committed `Prepared` call for capability setup.
    pub fn resume_prepared_call(&self) -> Result<PreparedModelCallRequest, ModelCallResumeFailure> {
        if !self.attempt_accepts_prepared_call() {
            return Err(ModelCallResumeFailure::AttemptIsNotPrepared);
        }
        let Some(call) = &self.current_call else {
            return Err(ModelCallResumeFailure::CallMissing);
        };
        if call.state() != CurrentModelCallState::Prepared {
            return Err(ModelCallResumeFailure::CallIsNotPrepared);
        }
        let origin_contents = self
            .frontier_entries
            .iter()
            .filter_map(|entry| match entry.payload() {
                SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
                | SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                    accepted_input, ..
                } => self
                    .origin_contents
                    .get(accepted_input)
                    .map(|content| (*accepted_input, content.clone())),
                SemanticTranscriptEntryPayload::TurnFailed { .. }
                | SemanticTranscriptEntryPayload::AssistantText { .. }
                | SemanticTranscriptEntryPayload::AssistantToolUse { .. }
                | SemanticTranscriptEntryPayload::ToolExecutionResult { .. }
                | SemanticTranscriptEntryPayload::ToolDenied { .. }
                | SemanticTranscriptEntryPayload::ToolClosed { .. }
                | SemanticTranscriptEntryPayload::TurnCompleted { .. }
                | SemanticTranscriptEntryPayload::TurnCancelled { .. } => None,
            })
            .collect();
        Ok(PreparedModelCallRequest {
            session: self.session,
            turn: self.turn,
            attempt: self.current_attempt.id(),
            call: call.clone(),
            frontier_entries: self.frontier_entries.clone(),
            origin_contents,
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
        let attempt = match self.current_attempt.state() {
            CurrentTurnAttemptState::Prepared => {
                self.current_attempt.clone().begin_running().map_err(|_| {
                    fail(
                        self.clone(),
                        ModelCallAuthorizationFailure::AttemptIsNotPrepared,
                    )
                })?
            }
            CurrentTurnAttemptState::Running if self.is_running_tool_continuation() => {
                self.current_attempt.clone()
            }
            CurrentTurnAttemptState::Running | CurrentTurnAttemptState::StopRequested { .. } => {
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

    /// Reconstructs the exact issued metadata after an ambiguous
    /// authorization commit without performing another state transition.
    pub fn resume_in_flight_call(&self) -> Option<AuthorizedModelCall> {
        let call = self.current_call.clone()?;
        if self.current_attempt.state() != &CurrentTurnAttemptState::Running
            || call.state() != CurrentModelCallState::InFlight
        {
            return None;
        }
        Some(AuthorizedModelCall {
            session: self.session,
            turn: self.turn,
            attempt: self.current_attempt.clone(),
            call,
            frontier_entries: self.frontier_entries.clone(),
            origin_contents: self.origin_contents.clone(),
        })
    }

    /// Reconstructs an issued call whose exact interrupt was durably accepted
    /// before this process entered the provider.
    pub fn resume_cancellation_requested_call(&self) -> Option<StopRequestedModelCallTurn> {
        let call = self.current_call.clone()?;
        let CurrentTurnAttemptState::StopRequested {
            causes: TurnAttemptStopCauses::CancellationOnly { interrupt },
        } = self.current_attempt.state()
        else {
            return None;
        };
        if call.state() != CurrentModelCallState::CancellationRequested {
            return None;
        }
        Some(StopRequestedModelCallTurn {
            session: self.session,
            turn: self.turn,
            call,
            attempt: self.current_attempt.clone(),
            interrupt: *interrupt,
        })
    }

    /// Applies one exactly correlated interrupt to the current initial-call
    /// execution.
    pub fn apply_interrupt(
        self,
        interrupt: AppliedInterruptCommandResult,
        identities: CancelledModelCallTurnIdentities,
    ) -> Result<ModelCallInterruptOutcome, ModelCallClosureError> {
        let proof = interrupt.proof();
        if interrupt.session() != self.session
            || proof.predecessor() != self.turn
            || interrupt.successor() == self.turn
            || interrupt.successor_order().priority()
                != (crate::AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                    predecessor: self.turn,
                })
        {
            return Err(ModelCallClosureError::InterruptCorrelationMismatch);
        }
        let unsent_call = matches!(
            (
                self.current_attempt.state(),
                self.current_call.as_ref().map(CurrentModelCall::state),
            ),
            (CurrentTurnAttemptState::Prepared, None)
                | (
                    CurrentTurnAttemptState::Prepared,
                    Some(CurrentModelCallState::Prepared)
                )
        ) || (self.is_running_tool_continuation()
            && matches!(
                self.current_call.as_ref().map(CurrentModelCall::state),
                None | Some(CurrentModelCallState::Prepared)
            ));
        if unsent_call {
            let reclassified_pending_steering = reclassify_pending_steering(
                &self.active_turn,
                &identities.pending_steering_reclassifications,
            )?;
            let ended_call = self
                .current_call
                .map(|call| call.end_cancelled_unsent(proof))
                .transpose()
                .map_err(|_| ModelCallClosureError::CallStateMismatch)?;
            let cancelled = close_cancelled_turn(
                ModelCallTurnScope {
                    session: self.session,
                    turn: self.turn,
                },
                self.current_attempt,
                ended_call,
                self.current_snapshot,
                proof,
                identities,
                reclassified_pending_steering,
            )?;
            return Ok(ModelCallInterruptOutcome::Cancelled(cancelled));
        }
        match (
            self.current_attempt.state(),
            self.current_call.as_ref().map(CurrentModelCall::state),
        ) {
            (CurrentTurnAttemptState::Running, Some(CurrentModelCallState::InFlight)) => {
                let attempt = self
                    .current_attempt
                    .request_cancellation(proof)
                    .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
                let call = self
                    .current_call
                    .ok_or(ModelCallClosureError::CallStateMismatch)?
                    .request_cancellation()
                    .map_err(|_| ModelCallClosureError::CallStateMismatch)?;
                Ok(ModelCallInterruptOutcome::CancellationRequested(
                    StopRequestedModelCallTurn {
                        session: self.session,
                        turn: self.turn,
                        call,
                        attempt,
                        interrupt: proof,
                    },
                ))
            }
            _ => Err(ModelCallClosureError::AttemptStateMismatch),
        }
    }

    fn attempt_accepts_prepared_call(&self) -> bool {
        self.current_attempt.state() == &CurrentTurnAttemptState::Prepared
            || self.is_running_tool_continuation()
    }

    fn is_running_tool_continuation(&self) -> bool {
        self.current_attempt.state() == &CurrentTurnAttemptState::Running
            && self.tool_continuation_frontier
    }

    /// Applies an interrupt after a tool batch has closed every logical
    /// request and no executor effect remains live.
    pub fn apply_interrupt_to_tool_batch(
        self,
        interrupt: AppliedInterruptCommandResult,
        result_projection: PreparedToolResultProjection,
        identities: CancelledModelCallTurnIdentities,
    ) -> Result<CancelledModelCallTurn, ModelCallClosureError> {
        if self.current_call.is_some()
            || interrupt.session() != self.session
            || result_projection.snapshot().frontier().owning_session() != self.session
            || result_projection.source_frontier() != self.current_snapshot.frontier().snapshot()
            || !self
                .current_snapshot
                .is_semantic_prefix_of(result_projection.snapshot())
            || result_projection.snapshot().entry_count()
                != self.current_snapshot.entry_count() + result_projection.entries().len()
        {
            return Err(ModelCallClosureError::InterruptCorrelationMismatch);
        }
        let reclassified_pending_steering = reclassify_pending_steering(
            &self.active_turn,
            &identities.pending_steering_reclassifications,
        )?;
        let (result_entries, result_snapshot) = result_projection.into_parts();
        let mut cancelled = close_cancelled_turn(
            ModelCallTurnScope {
                session: self.session,
                turn: self.turn,
            },
            self.current_attempt,
            None,
            result_snapshot,
            interrupt.proof(),
            identities,
            reclassified_pending_steering,
        )?;
        cancelled.tool_result_entries = result_entries;
        Ok(cancelled)
    }

    /// Applies one provider observation to freshly reloaded issued state.
    ///
    /// docs/spec/model-call-execution.md requires the observation
    /// transaction to reconstruct current authority after the provider
    /// effect rather than retaining the earlier authorization projection
    /// across that effect.
    pub fn apply_terminal_observation(
        self,
        observation: CorrelatedModelCallTerminalObservation,
        identities: ModelCallTerminalIdentities,
    ) -> Result<ModelCallTerminalOutcome, ModelCallClosureError> {
        let Some(call) = self.current_call else {
            return Err(ModelCallClosureError::CallStateMismatch);
        };
        if observation.correlation
            != (IssuedModelCallCorrelation {
                session: self.session,
                turn: self.turn,
                attempt: self.current_attempt.id(),
                call: call.id(),
                target: call.target(),
                frontier: call.frontier().snapshot(),
            })
        {
            return Err(ModelCallClosureError::ObservationCorrelationMismatch);
        }
        let lifecycle_valid = matches!(
            (self.current_attempt.state(), call.state()),
            (
                CurrentTurnAttemptState::Running,
                CurrentModelCallState::InFlight
            ) | (
                CurrentTurnAttemptState::StopRequested {
                    causes: TurnAttemptStopCauses::CancellationOnly { .. }
                },
                CurrentModelCallState::CancellationRequested
            )
        );
        if !lifecycle_valid {
            return Err(ModelCallClosureError::CallStateMismatch);
        }
        let cancellation_requested = matches!(
            self.current_attempt.state(),
            CurrentTurnAttemptState::StopRequested {
                causes: TurnAttemptStopCauses::CancellationOnly { .. }
            }
        );
        let reclassified_pending_steering = if (observation.observation.is_tool_round()
            && !cancellation_requested)
            || (observation.observation.disposition() == ModelCallDisposition::Ambiguous
                && !cancellation_requested)
        {
            Box::new([])
        } else {
            reclassify_pending_steering(
                &self.active_turn,
                identities.pending_steering_reclassifications(),
            )?
        };
        let dangerous_tool_auto_approval = self
            .configuration
            .effective()
            .dangerous_tool_auto_approval();
        apply_terminal_observation(
            ModelCallTurnScope {
                session: self.session,
                turn: self.turn,
            },
            self.current_attempt,
            call,
            self.frontier_entries,
            observation.observation,
            identities,
            ModelCallTerminalContext {
                reclassified_pending_steering,
                dangerous_tool_auto_approval,
            },
        )
    }

    /// Closes target-resolution failure before a model call exists.
    pub fn fail_target_resolution(
        self,
        resolution_error: ModelTargetResolutionError,
        identities: FailedModelCallTurnIdentities,
    ) -> Result<FailedModelCallTurn, ModelCallClosureError> {
        if self.current_call.is_some() {
            return Err(ModelCallClosureError::CallStateMismatch);
        }
        let frozen = *self.configuration.effective().model();
        if resolution_error.selection() != frozen
            || !matches!(self.targets.resolve(frozen), Err(expected) if expected == resolution_error)
        {
            return Err(ModelCallClosureError::TargetResolutionMismatch);
        }
        let reclassified_pending_steering = reclassify_pending_steering(
            &self.active_turn,
            &identities.pending_steering_reclassifications,
        )?;
        close_failed_turn(
            ModelCallTurnScope {
                session: self.session,
                turn: self.turn,
            },
            self.current_attempt,
            None,
            self.starting_snapshot,
            identities,
            UnstoppedAttemptDisposition::KnownFailure,
            reclassified_pending_steering,
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
        let reclassified_pending_steering = reclassify_pending_steering(
            &self.active_turn,
            &identities.pending_steering_reclassifications,
        )?;
        let ended_call = call
            .end_classified(ModelCallDisposition::KnownFailed)
            .map_err(|_| ModelCallClosureError::CallStateMismatch)?;
        close_failed_turn(
            ModelCallTurnScope {
                session: self.session,
                turn: self.turn,
            },
            self.current_attempt,
            Some(ended_call),
            self.current_snapshot,
            identities,
            UnstoppedAttemptDisposition::KnownFailure,
            reclassified_pending_steering,
        )
    }

    /// Applies the prior-process recovery rule in
    /// docs/spec/model-call-execution.md for a committed model call after
    /// startup has established that no provider task survived.
    pub fn recover_after_restart(
        self,
        failure_identities: FailedModelCallTurnIdentities,
    ) -> Result<ModelCallTerminalOutcome, ModelCallClosureError> {
        let Some(call) = self.current_call else {
            return Err(ModelCallClosureError::CallStateMismatch);
        };
        match call.state() {
            CurrentModelCallState::Prepared => {
                let reclassified_pending_steering = reclassify_pending_steering(
                    &self.active_turn,
                    &failure_identities.pending_steering_reclassifications,
                )?;
                let call = call
                    .end_classified(ModelCallDisposition::KnownFailed)
                    .map_err(|_| ModelCallClosureError::CallStateMismatch)?;
                close_failed_turn(
                    ModelCallTurnScope {
                        session: self.session,
                        turn: self.turn,
                    },
                    self.current_attempt,
                    Some(call),
                    self.current_snapshot,
                    failure_identities,
                    UnstoppedAttemptDisposition::Lost,
                    reclassified_pending_steering,
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
                let CurrentTurnAttemptState::StopRequested {
                    causes: TurnAttemptStopCauses::CancellationOnly { interrupt },
                } = self.current_attempt.state()
                else {
                    return Err(ModelCallClosureError::AttemptStateMismatch);
                };
                let proof = *interrupt;
                let call = call
                    .end_classified(ModelCallDisposition::Ambiguous)
                    .map_err(|_| ModelCallClosureError::CallStateMismatch)?;
                let call_id = call.id();
                let reclassified_pending_steering = reclassify_pending_steering(
                    &self.active_turn,
                    &failure_identities.pending_steering_reclassifications,
                )?;
                let terminal_snapshot = self
                    .current_snapshot
                    .derive_appending_candidate(failure_identities.terminal_frontier, Vec::new())
                    .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
                let attempt = self
                    .current_attempt
                    .end_after_cancellation(proof, CancellationStopDisposition::Lost)
                    .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
                let ambiguous_operations = NonEmptyIssuedOperationRefs::try_from_operations([
                    crate::IssuedOperationRef::ModelCall(call_id),
                ])
                .map_err(|_| ModelCallClosureError::AmbiguityConstructionFailed)?;
                let marker =
                    ReconciliationMarker::from_interrupt_ambiguity(ambiguous_operations, proof);
                Ok(ModelCallTerminalOutcome::ReconciliationRequired(
                    ReconciliationRequiredModelCallTurn {
                        session: self.session,
                        turn: self.turn,
                        call,
                        attempt,
                        disposition: TurnDisposition::ReconciliationRequired { marker },
                        terminal_snapshot,
                        reclassified_pending_steering,
                    },
                ))
            }
        }
    }

    /// Applies evidence-free startup recovery before any model call exists.
    ///
    /// Pending steering is reclassified in the same failed-terminal commit, so
    /// a prior-process prepared attempt cannot remain live solely because no
    /// model-call checkpoint had yet been created.
    pub fn recover_evidence_free_after_restart(
        self,
        failure_identities: FailedModelCallTurnIdentities,
    ) -> Result<FailedModelCallTurn, ModelCallClosureError> {
        if self.current_call.is_some() {
            return Err(ModelCallClosureError::CallStateMismatch);
        }
        let reclassified_pending_steering = reclassify_pending_steering(
            &self.active_turn,
            &failure_identities.pending_steering_reclassifications,
        )?;
        close_failed_turn(
            ModelCallTurnScope {
                session: self.session,
                turn: self.turn,
            },
            self.current_attempt,
            None,
            self.starting_snapshot,
            failure_identities,
            UnstoppedAttemptDisposition::Lost,
            reclassified_pending_steering,
        )
    }
}

/// Why a fresh prepared checkpoint could not be derived.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCallPreparationFailure {
    /// The frozen selection has no immutable configured target.
    TargetUnavailable,
    /// The initial call has already been durably created.
    CallAlreadyExists,
    /// The current physical attempt is no longer prepared.
    AttemptIsNotPrepared,
    /// The supplied steering entry count differs from the complete inventory.
    SteeringIdentityCountMismatch,
    /// The steering snapshot candidate is missing, unexpected, or invalid.
    SteeringFrontierIdentityMismatch,
    /// Pending steering cannot form the exact consumed semantic suffix.
    SteeringCorrelationMismatch,
}

/// Failed preparation retaining the unchanged live aggregate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCallPreparationError {
    execution: Box<ModelCallExecution>,
    failure: ModelCallPreparationFailure,
    target_resolution_error: Option<ModelTargetResolutionError>,
}

impl ModelCallPreparationError {
    fn new(execution: ModelCallExecution, failure: ModelCallPreparationFailure) -> Self {
        Self {
            execution: Box::new(execution),
            failure,
            target_resolution_error: None,
        }
    }

    fn target_unavailable(
        execution: ModelCallExecution,
        target_resolution_error: ModelTargetResolutionError,
    ) -> Self {
        Self {
            execution: Box::new(execution),
            failure: ModelCallPreparationFailure::TargetUnavailable,
            target_resolution_error: Some(target_resolution_error),
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

    /// Returns the exact immutable-catalog miss for target unavailability.
    pub const fn target_resolution_error(&self) -> Option<ModelTargetResolutionError> {
        self.target_resolution_error
    }
}

/// A newly prepared call and the exact durable ownership facts to commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedInitialModelCall {
    session: SessionId,
    turn: TurnId,
    attempt: TurnAttemptId,
    call: CurrentModelCall,
    consumed_steering: Box<[PreparedSteeringConsumption]>,
    steering_snapshot: Option<ResolvedContextFrontierSnapshot>,
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

    /// Returns every steering consumption in immutable acceptance order.
    pub fn consumed_steering(&self) -> &[PreparedSteeringConsumption] {
        &self.consumed_steering
    }

    /// Borrows the extended call frontier when steering created one.
    pub const fn steering_snapshot(&self) -> Option<&ResolvedContextFrontierSnapshot> {
        self.steering_snapshot.as_ref()
    }
}

/// One accepted-input disposition and semantic entry prepared atomically.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedSteeringConsumption {
    accepted_input: AcceptedInputLifecycle,
    semantic_entry: SemanticTranscriptEntry,
}

impl PreparedSteeringConsumption {
    /// Borrows the consumed accepted-input lifecycle.
    pub const fn accepted_input(&self) -> &AcceptedInputLifecycle {
        &self.accepted_input
    }

    /// Borrows the semantic entry appended for this consumption.
    pub const fn semantic_entry(&self) -> &SemanticTranscriptEntry {
        &self.semantic_entry
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

    /// Returns the sealed issued facts that bind later provider observations
    /// to this exact authorization.
    pub const fn observation_correlation(&self) -> IssuedModelCallCorrelation {
        IssuedModelCallCorrelation {
            session: self.session,
            turn: self.turn,
            attempt: self.attempt.id(),
            call: self.call.id(),
            target: self.call.target(),
            frontier: self.call.frontier().snapshot(),
        }
    }
}

/// Sealed issued-call facts carried across one provider interaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IssuedModelCallCorrelation {
    session: SessionId,
    turn: TurnId,
    attempt: TurnAttemptId,
    call: ModelCallId,
    target: ResolvedProviderTarget,
    frontier: ContextFrontierId,
}

impl IssuedModelCallCorrelation {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the owning logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the exact issued physical attempt.
    pub const fn attempt(&self) -> TurnAttemptId {
        self.attempt
    }

    /// Returns the exact issued model call.
    pub const fn call(&self) -> ModelCallId {
        self.call
    }

    /// Returns the exact pinned target used by the issued call.
    pub const fn target(&self) -> ResolvedProviderTarget {
        self.target
    }

    /// Returns the exact context frontier used by the issued call.
    pub const fn frontier(&self) -> ContextFrontierId {
        self.frontier
    }

    /// Binds one provider-neutral terminal observation to these issued facts.
    pub fn bind_terminal_observation(
        self,
        observation: ModelCallTerminalObservation,
    ) -> CorrelatedModelCallTerminalObservation {
        CorrelatedModelCallTerminalObservation {
            correlation: self,
            observation,
        }
    }
}

/// One provider-neutral terminal observation bound to exact issued authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CorrelatedModelCallTerminalObservation {
    correlation: IssuedModelCallCorrelation,
    observation: ModelCallTerminalObservation,
}

impl CorrelatedModelCallTerminalObservation {
    /// Returns the exact model call named by the issued correlation.
    pub const fn call(&self) -> ModelCallId {
        self.correlation.call
    }

    /// Borrows all exact issued facts carried with the observation.
    pub const fn correlation(&self) -> &IssuedModelCallCorrelation {
        &self.correlation
    }

    /// Borrows the provider-neutral physical outcome.
    pub const fn observation(&self) -> &ModelCallTerminalObservation {
        &self.observation
    }
}

#[derive(Clone, Copy)]
struct ModelCallTurnScope {
    session: SessionId,
    turn: TurnId,
}

struct ModelCallTerminalContext {
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
    dangerous_tool_auto_approval: DangerousToolAutoApproval,
}

fn apply_terminal_observation(
    scope: ModelCallTurnScope,
    attempt: CurrentTurnAttempt,
    call: CurrentModelCall,
    frontier_entries: Box<[SemanticTranscriptEntry]>,
    observation: ModelCallTerminalObservation,
    identities: ModelCallTerminalIdentities,
    context: ModelCallTerminalContext,
) -> Result<ModelCallTerminalOutcome, ModelCallClosureError> {
    let ModelCallTurnScope { session, turn } = scope;
    let ModelCallTerminalContext {
        reclassified_pending_steering,
        dangerous_tool_auto_approval,
    } = context;
    let cancellation_proof = match attempt.state() {
        CurrentTurnAttemptState::StopRequested {
            causes: TurnAttemptStopCauses::CancellationOnly { interrupt },
        } => Some(*interrupt),
        CurrentTurnAttemptState::Prepared
        | CurrentTurnAttemptState::Running
        | CurrentTurnAttemptState::StopRequested {
            causes: TurnAttemptStopCauses::FatalMismatch(_),
        } => None,
    };
    let disposition = observation.disposition();
    let source_frontier = call.frontier();
    let ended_call = call
        .end_classified(disposition)
        .map_err(|_| ModelCallClosureError::CallStateMismatch)?;
    match observation {
        ModelCallTerminalObservation::Completed { assistant_text } => {
            let ModelCallTerminalIdentities::Completed(identities) = identities else {
                return Err(ModelCallClosureError::IdentityShapeMismatch);
            };
            let ended_attempt = match cancellation_proof {
                Some(proof) => attempt
                    .end_after_cancellation(proof, CancellationStopDisposition::TurnCompleted),
                None => attempt.end_without_stop(UnstoppedAttemptDisposition::TurnCompleted),
            }
            .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
            let completed = complete_turn(
                scope,
                ended_call,
                ended_attempt,
                frontier_entries.into_vec(),
                assistant_text,
                identities,
                reclassified_pending_steering,
            )?;
            Ok(ModelCallTerminalOutcome::Completed(completed))
        }
        ModelCallTerminalObservation::CompletedWithTools { response } => {
            if let Some(proof) = cancellation_proof {
                let ModelCallTerminalIdentities::StoppedToolRound(identities) = identities else {
                    return Err(ModelCallClosureError::IdentityShapeMismatch);
                };
                let ended_attempt = attempt
                    .end_after_cancellation(proof, CancellationStopDisposition::Cancelled)
                    .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
                return assemble_stopped_tool_round(
                    scope,
                    ended_call,
                    ended_attempt,
                    frontier_entries.into_vec(),
                    response,
                    proof,
                    identities,
                    reclassified_pending_steering,
                )
                .map(ModelCallTerminalOutcome::CancelledWithToolResponse);
            }
            let ModelCallTerminalIdentities::ToolRound(identities) = identities else {
                return Err(ModelCallClosureError::IdentityShapeMismatch);
            };
            let ended_attempt = attempt
                .end_without_stop(UnstoppedAttemptDisposition::YieldedToDurableWait)
                .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
            assemble_tool_round(
                scope,
                ended_call,
                ended_attempt,
                frontier_entries.into_vec(),
                response,
                identities,
                dangerous_tool_auto_approval,
            )
            .map(ModelCallTerminalOutcome::ToolRound)
        }
        ModelCallTerminalObservation::KnownFailed => {
            let ModelCallTerminalIdentities::Failed(identities) = identities else {
                return Err(ModelCallClosureError::IdentityShapeMismatch);
            };
            let source = ResolvedContextFrontierSnapshot::try_from_candidate(
                session,
                source_frontier.snapshot(),
                frontier_entries
                    .iter()
                    .map(SemanticTranscriptEntry::reference)
                    .collect(),
            )
            .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
            let failed = match cancellation_proof {
                Some(proof) => close_failed_turn_after_cancellation(
                    scope,
                    attempt,
                    ended_call,
                    source,
                    proof,
                    identities,
                    reclassified_pending_steering,
                ),
                None => close_failed_turn(
                    scope,
                    attempt,
                    Some(ended_call),
                    source,
                    identities,
                    UnstoppedAttemptDisposition::KnownFailure,
                    reclassified_pending_steering,
                ),
            }?;
            Ok(ModelCallTerminalOutcome::Failed(failed))
        }
        ModelCallTerminalObservation::Cancelled => match cancellation_proof {
            Some(proof) => {
                let ModelCallTerminalIdentities::PhysicalCancellation(identities) = identities
                else {
                    return Err(ModelCallClosureError::IdentityShapeMismatch);
                };
                let identities = CancelledModelCallTurnIdentities {
                    cancellation_entry: identities.terminal_entry,
                    terminal_frontier: identities.terminal_frontier,
                    pending_steering_reclassifications: identities
                        .pending_steering_reclassifications,
                };
                let source = ResolvedContextFrontierSnapshot::try_from_candidate(
                    session,
                    source_frontier.snapshot(),
                    frontier_entries
                        .iter()
                        .map(SemanticTranscriptEntry::reference)
                        .collect(),
                )
                .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
                close_cancelled_turn(
                    scope,
                    attempt,
                    Some(ended_call),
                    source,
                    proof,
                    identities,
                    reclassified_pending_steering,
                )
                .map(ModelCallTerminalOutcome::Cancelled)
            }
            None => {
                let ModelCallTerminalIdentities::PhysicalCancellation(identities) = identities
                else {
                    return Err(ModelCallClosureError::IdentityShapeMismatch);
                };
                let identities = FailedModelCallTurnIdentities {
                    failure_entry: identities.terminal_entry,
                    terminal_frontier: identities.terminal_frontier,
                    pending_steering_reclassifications: identities
                        .pending_steering_reclassifications,
                };
                let source = ResolvedContextFrontierSnapshot::try_from_candidate(
                    session,
                    source_frontier.snapshot(),
                    frontier_entries
                        .iter()
                        .map(SemanticTranscriptEntry::reference)
                        .collect(),
                )
                .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
                close_failed_turn(
                    scope,
                    attempt,
                    Some(ended_call),
                    source,
                    identities,
                    UnstoppedAttemptDisposition::KnownFailure,
                    reclassified_pending_steering,
                )
                .map(ModelCallTerminalOutcome::Failed)
            }
        },
        ModelCallTerminalObservation::Refused => {
            let ModelCallTerminalIdentities::Refused(identities) = identities else {
                return Err(ModelCallClosureError::IdentityShapeMismatch);
            };
            let ended_attempt = match cancellation_proof {
                Some(proof) => {
                    attempt.end_after_cancellation(proof, CancellationStopDisposition::TurnRefused)
                }
                None => attempt.end_without_stop(UnstoppedAttemptDisposition::TurnRefused),
            }
            .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
            let source = ResolvedContextFrontierSnapshot::try_from_candidate(
                session,
                source_frontier.snapshot(),
                frontier_entries
                    .iter()
                    .map(SemanticTranscriptEntry::reference)
                    .collect(),
            )
            .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
            let terminal_snapshot = source
                .derive_appending_candidate(identities.terminal_frontier, Vec::new())
                .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
            Ok(ModelCallTerminalOutcome::Refused(RefusedModelCallTurn {
                session,
                turn,
                call: ended_call,
                attempt: ended_attempt,
                disposition: TurnDisposition::Refused,
                terminal_snapshot,
                reclassified_pending_steering,
            }))
        }
        ModelCallTerminalObservation::Ambiguous => {
            let ModelCallTerminalIdentities::Ambiguous(identities) = identities else {
                return Err(ModelCallClosureError::IdentityShapeMismatch);
            };
            let call_id = ended_call.id();
            let ended_attempt = match cancellation_proof {
                Some(proof) => {
                    attempt.end_after_cancellation(proof, CancellationStopDisposition::Ambiguous)
                }
                None => attempt.end_without_stop(UnstoppedAttemptDisposition::Ambiguous),
            }
            .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
            let ambiguous_operations = NonEmptyIssuedOperationRefs::try_from_operations([
                crate::IssuedOperationRef::ModelCall(call_id),
            ])
            .map_err(|_| ModelCallClosureError::AmbiguityConstructionFailed)?;
            if let Some(proof) = cancellation_proof {
                let source = ResolvedContextFrontierSnapshot::try_from_candidate(
                    session,
                    source_frontier.snapshot(),
                    frontier_entries
                        .iter()
                        .map(SemanticTranscriptEntry::reference)
                        .collect(),
                )
                .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
                let terminal_snapshot = source
                    .derive_appending_candidate(identities.terminal_frontier, Vec::new())
                    .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
                let marker =
                    ReconciliationMarker::from_interrupt_ambiguity(ambiguous_operations, proof);
                Ok(ModelCallTerminalOutcome::ReconciliationRequired(
                    ReconciliationRequiredModelCallTurn {
                        session,
                        turn,
                        call: ended_call,
                        attempt: ended_attempt,
                        disposition: TurnDisposition::ReconciliationRequired { marker },
                        terminal_snapshot,
                        reclassified_pending_steering,
                    },
                ))
            } else {
                Ok(ModelCallTerminalOutcome::AwaitingRecovery(
                    AmbiguousModelCallTurn {
                        session,
                        turn,
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
    /// Definitive success whose ordered response contains tool proposals.
    CompletedWithTools {
        /// Ordered text and normalized proposals, proven to contain a tool.
        response: ToolUsingAssistantResponse,
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
            Self::Completed { .. } | Self::CompletedWithTools { .. } => {
                ModelCallDisposition::Completed
            }
            Self::KnownFailed => ModelCallDisposition::KnownFailed,
            Self::Refused => ModelCallDisposition::Refused,
            Self::Cancelled => ModelCallDisposition::Cancelled,
            Self::Ambiguous => ModelCallDisposition::Ambiguous,
        }
    }

    const fn is_tool_round(&self) -> bool {
        matches!(self, Self::CompletedWithTools { .. })
    }
}

/// One fresh turn identity correlated to an exact pending steering input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingSteeringReclassificationIdentity {
    accepted_input: AcceptedInputId,
    turn: TurnId,
}

impl PendingSteeringReclassificationIdentity {
    /// Associates one pending accepted input with its proposed successor turn.
    pub const fn new(accepted_input: AcceptedInputId, turn: TurnId) -> Self {
        Self {
            accepted_input,
            turn,
        }
    }

    /// Returns the pending accepted input being reclassified.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Returns the fresh turn proposed for that input.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
}

/// Fresh identities for a successful text-only outcome transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletedModelCallIdentities {
    assistant_entries: Vec<SemanticTranscriptEntryId>,
    completion_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
    pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
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
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one fresh successor identity per pending steering input, in
    /// session acceptance order.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }
}

/// Fresh identities and initial policy for one ordered tool-response part.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolResponsePartIdentity {
    /// One semantic assistant-text entry.
    Text {
        /// Fresh semantic-entry identity.
        entry: SemanticTranscriptEntryId,
    },
    /// One logical request plus its reference-only semantic entry.
    ToolCall {
        /// Fresh semantic-entry identity.
        entry: SemanticTranscriptEntryId,
        /// Fresh logical request identity.
        request: ToolRequestId,
        /// The explicit initial approval outcome selected by application policy.
        approval: InitialToolApproval,
    },
}

impl ToolResponsePartIdentity {
    /// Constructs a text-part identity.
    pub const fn text(entry: SemanticTranscriptEntryId) -> Self {
        Self::Text { entry }
    }

    /// Constructs a tool-part identity and explicit initial policy outcome.
    pub const fn tool_call(
        entry: SemanticTranscriptEntryId,
        request: ToolRequestId,
        approval: InitialToolApproval,
    ) -> Self {
        Self::ToolCall {
            entry,
            request,
            approval,
        }
    }
}

/// Fresh identities for one nonterminal tool-using response commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRoundModelCallIdentities {
    response_parts: Vec<ToolResponsePartIdentity>,
    yielded_frontier: ContextFrontierId,
    continuation_attempt: Option<TurnAttemptId>,
}

impl ToolRoundModelCallIdentities {
    /// Supplies one identity shape per response part, a yielded frontier, and
    /// a continuation attempt exactly when every request is auto-approved.
    pub fn new(
        response_parts: Vec<ToolResponsePartIdentity>,
        yielded_frontier: ContextFrontierId,
        continuation_attempt: Option<TurnAttemptId>,
    ) -> Self {
        Self {
            response_parts,
            yielded_frontier,
            continuation_attempt,
        }
    }

    /// Returns ordered response-part identities.
    pub fn response_parts(&self) -> &[ToolResponsePartIdentity] {
        &self.response_parts
    }

    /// Returns the proposed yielded snapshot identity.
    pub const fn yielded_frontier(&self) -> ContextFrontierId {
        self.yielded_frontier
    }

    /// Returns the proposed continuation attempt, if the batch has no wait.
    pub const fn continuation_attempt(&self) -> Option<TurnAttemptId> {
        self.continuation_attempt
    }
}

/// Fresh identities for one response part when an applied interrupt closes
/// newly proposed tools instead of continuing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoppedToolResponsePartIdentity {
    /// One semantic assistant-text entry.
    Text {
        /// Fresh semantic-entry identity.
        entry: SemanticTranscriptEntryId,
    },
    /// One request, tool-use entry, and turn-closed result entry.
    ToolCall {
        /// Fresh assistant tool-use entry identity.
        entry: SemanticTranscriptEntryId,
        /// Fresh logical request identity.
        request: ToolRequestId,
        /// Fresh reference-only closed-result entry identity.
        closed_result_entry: SemanticTranscriptEntryId,
    },
}

impl StoppedToolResponsePartIdentity {
    /// Constructs one text identity.
    pub const fn text(entry: SemanticTranscriptEntryId) -> Self {
        Self::Text { entry }
    }

    /// Constructs one closed tool-proposal identity group.
    pub const fn tool_call(
        entry: SemanticTranscriptEntryId,
        request: ToolRequestId,
        closed_result_entry: SemanticTranscriptEntryId,
    ) -> Self {
        Self::ToolCall {
            entry,
            request,
            closed_result_entry,
        }
    }
}

/// Fresh identities for a tool-using response closed by an applied interrupt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoppedToolRoundModelCallIdentities {
    response_parts: Vec<StoppedToolResponsePartIdentity>,
    cancellation_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
    pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

impl StoppedToolRoundModelCallIdentities {
    /// Supplies ordered response identities, the cancellation marker, and
    /// terminal snapshot.
    pub fn new(
        response_parts: Vec<StoppedToolResponsePartIdentity>,
        cancellation_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            response_parts,
            cancellation_entry,
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one successor identity per pending steering input.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }
}

/// Fresh identities for a failed-turn outcome transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailedModelCallTurnIdentities {
    failure_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
    pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

impl FailedModelCallTurnIdentities {
    /// Supplies the failure marker and terminal-frontier identities.
    pub fn new(
        failure_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            failure_entry,
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one fresh successor identity per pending steering input, in
    /// session acceptance order.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }
}

/// Fresh identities for an interrupt-cancelled turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelledModelCallTurnIdentities {
    cancellation_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
    pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

/// Fresh identities for a physical-cancellation observation.
///
/// The freshly reloaded attempt decides whether the terminal entry is a
/// proof-bearing cancellation marker or an ordinary failure marker. This
/// shape lets the application mint one collision domain without guessing
/// whether a concurrent interrupt committed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhysicalCancellationModelCallTurnIdentities {
    terminal_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
    pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

impl PhysicalCancellationModelCallTurnIdentities {
    /// Supplies the terminal-marker and terminal-frontier identities.
    pub fn new(
        terminal_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            terminal_entry,
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one fresh successor identity per pending steering input, in
    /// session acceptance order.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }
}

impl CancelledModelCallTurnIdentities {
    /// Supplies the cancellation marker and terminal-frontier identities.
    pub fn new(
        cancellation_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            cancellation_entry,
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one fresh successor identity per pending steering input, in
    /// session acceptance order.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }

    /// Reuses the terminal frontier and pending-steering successors when an
    /// interrupt closes an existing ambiguity wait instead of emitting a
    /// cancellation marker.
    pub fn into_ambiguous(self) -> AmbiguousModelCallTurnIdentities {
        AmbiguousModelCallTurnIdentities {
            terminal_frontier: self.terminal_frontier,
            pending_steering_reclassifications: self.pending_steering_reclassifications,
        }
    }
}

/// Fresh identity for a refusal terminal frontier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefusedModelCallTurnIdentities {
    terminal_frontier: ContextFrontierId,
    pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

impl RefusedModelCallTurnIdentities {
    /// Supplies the new equal-content terminal frontier identity.
    pub fn new(terminal_frontier: ContextFrontierId) -> Self {
        Self {
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one fresh successor identity per pending steering input, in
    /// session acceptance order.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }
}

/// Candidate identities matching one possible terminal observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallTerminalIdentities {
    /// Successful assistant-content and completion identities.
    Completed(CompletedModelCallIdentities),
    /// Assistant content, logical requests, and a nonterminal yielded frontier.
    ToolRound(ToolRoundModelCallIdentities),
    /// Tool response content and closed results under an applied interrupt.
    StoppedToolRound(StoppedToolRoundModelCallIdentities),
    /// Known-failure or cause-free physical-cancellation identities.
    Failed(FailedModelCallTurnIdentities),
    /// Physical-cancellation identities whose semantic meaning is selected
    /// from the freshly reloaded stop state.
    PhysicalCancellation(PhysicalCancellationModelCallTurnIdentities),
    /// Refusal terminal-frontier identity.
    Refused(RefusedModelCallTurnIdentities),
    /// Ambiguity identities used only when a stop requires terminal
    /// reconciliation; ordinary ambiguity ignores them while retaining the
    /// slot.
    Ambiguous(AmbiguousModelCallTurnIdentities),
}

/// One terminal or durable-wait result from the observation transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallTerminalOutcome {
    /// Assistant content and turn completion committed atomically.
    Completed(CompletedModelCallTurn),
    /// Assistant content and requests committed while the same turn continues.
    ToolRound(ToolRoundModelCallTurn),
    /// A tool-using response raced an interrupt and closed without execution.
    CancelledWithToolResponse(CancelledToolRoundModelCallTurn),
    /// The call and turn failed atomically.
    Failed(FailedModelCallTurn),
    /// The applied interrupt and physical evidence cancelled the turn.
    Cancelled(CancelledModelCallTurn),
    /// The provider refusal terminalized the turn.
    Refused(RefusedModelCallTurn),
    /// An applied interrupt and exact ambiguity set require reconciliation.
    ReconciliationRequired(ReconciliationRequiredModelCallTurn),
    /// Physical ambiguity ended the attempt and retained the slot.
    AwaitingRecovery(AmbiguousModelCallTurn),
}

/// Result of atomically applying one matching interrupt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallInterruptOutcome {
    /// Unsent work ended directly and released the turn slot.
    Cancelled(CancelledModelCallTurn),
    /// Issued work retained the slot while durable cancellation was requested.
    CancellationRequested(StopRequestedModelCallTurn),
    /// An existing physical-ambiguity wait closed under the applied interrupt.
    ReconciliationRequired(ReconciliationRequiredModelCallTurn),
    /// An existing tool-attempt ambiguity closed under the applied interrupt.
    ToolReconciliationRequired(ReconciliationRequiredToolTurn),
}

impl ModelCallTerminalIdentities {
    fn pending_steering_reclassifications(&self) -> &[PendingSteeringReclassificationIdentity] {
        match self {
            Self::Completed(identities) => &identities.pending_steering_reclassifications,
            Self::ToolRound(_) => &[],
            Self::StoppedToolRound(identities) => &identities.pending_steering_reclassifications,
            Self::Failed(identities) => &identities.pending_steering_reclassifications,
            Self::PhysicalCancellation(identities) => {
                &identities.pending_steering_reclassifications
            }
            Self::Refused(identities) => &identities.pending_steering_reclassifications,
            Self::Ambiguous(identities) => &identities.pending_steering_reclassifications,
        }
    }
}

/// Fresh identities needed only when ambiguity terminalizes under a stop.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AmbiguousModelCallTurnIdentities {
    terminal_frontier: ContextFrontierId,
    pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

impl AmbiguousModelCallTurnIdentities {
    /// Supplies the candidate terminal frontier for proof-bearing
    /// reconciliation.
    pub const fn new(terminal_frontier: ContextFrontierId) -> Self {
        Self {
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one fresh successor identity per pending steering input.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }
}

/// One pending steering input atomically reclassified when its source turn
/// terminalizes before another model-call safe point.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReclassifiedPendingSteeringTurn {
    session: SessionId,
    source_turn: TurnId,
    accepted_input: AcceptedInputLifecycle,
    turn: TurnId,
    order: AcceptedInputQueueOrder,
    binding: SteeringBinding,
    effective_configuration: EffectiveConfiguration,
}

impl ReclassifiedPendingSteeringTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the terminal source turn.
    pub const fn source_turn(&self) -> TurnId {
        self.source_turn
    }

    /// Borrows the accepted input with its reclassified disposition.
    pub const fn accepted_input(&self) -> &AcceptedInputLifecycle {
        &self.accepted_input
    }

    /// Returns the fresh queued turn originated by the input.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns ordinary queue order at the input's original position.
    pub const fn order(&self) -> AcceptedInputQueueOrder {
        self.order
    }

    /// Returns inherited provenance binding the new origin to its source.
    pub const fn binding(&self) -> SteeringBinding {
        self.binding
    }

    /// Borrows the source turn's exact inherited effective configuration.
    pub const fn effective_configuration(&self) -> &EffectiveConfiguration {
        &self.effective_configuration
    }
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
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

/// One nonterminal commit candidate from a tool-using completed model call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRoundModelCallTurn {
    session: SessionId,
    turn: TurnId,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    assistant_entries: Box<[SemanticTranscriptEntry]>,
    requests: Box<[ToolRequest]>,
    automatic_approvals: Box<[ToolApprovalResolution]>,
    yielded_snapshot: ResolvedContextFrontierSnapshot,
    next_phase: ActiveTurnPhase,
}

impl ToolRoundModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the continuing logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Borrows the completed producing call.
    pub const fn call(&self) -> &EndedModelCall {
        &self.call
    }

    /// Borrows the yielded producing attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }

    /// Returns ordered text/tool-use semantic entries.
    pub fn assistant_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.assistant_entries
    }

    /// Returns logical requests in proposal order.
    pub fn requests(&self) -> &[ToolRequest] {
        &self.requests
    }

    /// Returns only automatic decisions, in proposal order among those selected.
    pub fn automatic_approvals(&self) -> &[ToolApprovalResolution] {
        &self.automatic_approvals
    }

    /// Borrows the source-plus-assistant yielded snapshot.
    pub const fn yielded_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.yielded_snapshot
    }

    /// Borrows the approval wait or prepared continuation attempt.
    pub const fn next_phase(&self) -> &ActiveTurnPhase {
        &self.next_phase
    }
}

/// One interrupt-cancelled turn whose racing response proposed tools.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelledToolRoundModelCallTurn {
    session: SessionId,
    turn: TurnId,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    disposition: TurnDisposition,
    assistant_entries: Box<[SemanticTranscriptEntry]>,
    requests: Box<[ToolRequest]>,
    closed_result_entries: Box<[SemanticTranscriptEntry]>,
    cancellation_entry: SemanticTranscriptEntry,
    terminal_snapshot: ResolvedContextFrontierSnapshot,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

impl CancelledToolRoundModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the cancelled logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Borrows the completed producing call.
    pub const fn call(&self) -> &EndedModelCall {
        &self.call
    }

    /// Borrows the proof-bearing ended attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }

    /// Borrows the cancelled disposition.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }

    /// Returns ordered assistant response entries.
    pub fn assistant_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.assistant_entries
    }

    /// Returns proposed logical requests in proposal order.
    pub fn requests(&self) -> &[ToolRequest] {
        &self.requests
    }

    /// Returns proposal-ordered closed-result entries.
    pub fn closed_result_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.closed_result_entries
    }

    /// Borrows the final proof-bearing cancellation marker.
    pub const fn cancellation_entry(&self) -> &SemanticTranscriptEntry {
        &self.cancellation_entry
    }

    /// Borrows the complete terminal snapshot.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }

    /// Returns successor turns for pending steering.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }
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
    /// Returns queued turns created from every pending steering input.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
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
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
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
    /// Returns queued turns created from every pending steering input.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }
}

/// One interrupt-cancelled turn commit candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelledModelCallTurn {
    session: SessionId,
    turn: TurnId,
    call: Option<EndedModelCall>,
    attempt: EndedTurnAttempt,
    disposition: TurnDisposition,
    tool_result_entries: Box<[SemanticTranscriptEntry]>,
    cancellation_entry: SemanticTranscriptEntry,
    terminal_snapshot: ResolvedContextFrontierSnapshot,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

impl CancelledModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the cancelled turn.
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
    /// Borrows the proof-bearing cancelled disposition.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }
    /// Borrows proposal-ordered tool results materialized before cancellation.
    pub fn tool_result_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.tool_result_entries
    }
    /// Borrows the explicit cancellation marker.
    pub const fn cancellation_entry(&self) -> &SemanticTranscriptEntry {
        &self.cancellation_entry
    }
    /// Borrows the complete terminal frontier.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }
    /// Returns queued turns created from every pending steering input.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }
}

/// One durable cancellation request retaining the active turn slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StopRequestedModelCallTurn {
    session: SessionId,
    turn: TurnId,
    call: CurrentModelCall,
    attempt: CurrentTurnAttempt,
    interrupt: AppliedInterruptProof,
}

impl StopRequestedModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the stopped active turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the exact cancellation-requested call.
    pub const fn call(&self) -> &CurrentModelCall {
        &self.call
    }
    /// Borrows the proof-bearing stop-requested attempt.
    pub const fn attempt(&self) -> &CurrentTurnAttempt {
        &self.attempt
    }
    /// Returns the applied interrupt authorizing cancellation.
    pub const fn interrupt(&self) -> AppliedInterruptProof {
        self.interrupt
    }

    /// Returns the issued facts binding a provider-neutral cancellation
    /// observation to this exact stopped authorization.
    pub const fn observation_correlation(&self) -> IssuedModelCallCorrelation {
        IssuedModelCallCorrelation {
            session: self.session,
            turn: self.turn,
            attempt: self.attempt.id(),
            call: self.call.id(),
            target: self.call.target(),
            frontier: self.call.frontier().snapshot(),
        }
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
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
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
    /// Returns queued turns created from every pending steering input.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }
}

/// One proof-bearing reconciliation-required commit candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconciliationRequiredModelCallTurn {
    session: SessionId,
    turn: TurnId,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    disposition: TurnDisposition,
    terminal_snapshot: ResolvedContextFrontierSnapshot,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

impl ReconciliationRequiredModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the turn whose ambiguity requires reconciliation.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the exact ambiguous physical call.
    pub const fn call(&self) -> &EndedModelCall {
        &self.call
    }
    /// Borrows the proof-bearing ended attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }
    /// Borrows the exact reconciliation disposition and marker.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }
    /// Borrows the terminal frontier.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }
    /// Returns queued turns created from every pending steering input.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }
}

/// One proof-bearing tool-attempt reconciliation commit candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconciliationRequiredToolTurn {
    session: SessionId,
    turn: TurnId,
    tool_attempt: EndedToolAttempt,
    attempt: EndedTurnAttempt,
    disposition: TurnDisposition,
    tool_result_entries: Box<[SemanticTranscriptEntry]>,
    terminal_snapshot: ResolvedContextFrontierSnapshot,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

impl ReconciliationRequiredToolTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the turn whose ambiguity requires reconciliation.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the exact ambiguous physical tool attempt.
    pub const fn tool_attempt(&self) -> &EndedToolAttempt {
        &self.tool_attempt
    }
    /// Borrows the proof-bearing ended turn attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }
    /// Borrows the exact reconciliation disposition and marker.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }
    /// Returns proposal-ordered logical results closing the terminal batch.
    pub fn tool_result_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.tool_result_entries
    }
    /// Borrows the prefix-extending terminal frontier.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }
    /// Returns queued turns created from every pending steering input.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
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
    /// The observation names different issued authority than fresh state.
    ObservationCorrelationMismatch,
    /// The applied interrupt does not name this exact session, predecessor,
    /// and immediate successor relation.
    InterruptCorrelationMismatch,
    /// The attempt cannot take the required terminal transition.
    AttemptStateMismatch,
    /// Claimed target-resolution failure does not match this execution's
    /// immutable catalog and frozen selection.
    TargetResolutionMismatch,
    /// Assistant text and entry identity counts differ.
    AssistantIdentityCountMismatch,
    /// Tool response parts and their identity shapes differ.
    ToolResponseIdentityMismatch,
    /// The request ordinal cannot fit the durable zero-based space.
    ToolRequestOrdinalOverflow,
    /// Initial approval provenance contradicts the frozen blanket posture.
    InitialToolApprovalMismatch,
    /// A continuation attempt was missing, unexpected, or reused the yielded attempt.
    ContinuationAttemptIdentityMismatch,
    /// Pending steering and proposed successor identities are not exact,
    /// ordered, distinct, and source-turn-safe.
    PendingSteeringReclassificationMismatch,
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
    let ActiveTurnPhase::Running { current_attempt } = input.active_turn.phase() else {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::TurnIsNotRunning,
        ));
    };
    let current_attempt = current_attempt.clone();
    let session = input.active_turn.session();
    let turn = input.active_turn.turn();
    let configuration = input.active_turn.configuration().clone();
    let start = input.active_turn.start();
    if input.starting_snapshot.frontier().owning_session() != session {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::StartingSnapshotSessionMismatch,
        ));
    }
    if start.frontier() != input.starting_snapshot.frontier() {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::StartingSnapshotMismatch,
        ));
    }
    if input.calls.len() > 1 {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::MultipleCalls,
        ));
    }
    let current_snapshot = match (
        input.calls.first(),
        input.call_snapshot.as_ref(),
        input.continuation_snapshot.as_ref(),
    ) {
        (None, None, None) => input.starting_snapshot.clone(),
        (None, Some(_), _) => {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::CallSnapshotUnexpected,
            ));
        }
        (None, None, Some(stored)) => {
            let Some(current) = stored.clone().reconstitute() else {
                return Err(fail(
                    input,
                    ModelCallExecutionReconstitutionFailure::ContinuationSnapshotMismatch,
                ));
            };
            if current.frontier().owning_session() != session
                || current.frontier() == input.starting_snapshot.frontier()
                || !input.starting_snapshot.is_semantic_prefix_of(&current)
            {
                return Err(fail(
                    input,
                    ModelCallExecutionReconstitutionFailure::ContinuationSnapshotMismatch,
                ));
            }
            current
        }
        (Some(_), _, Some(_)) => {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::ContinuationSnapshotUnexpected,
            ));
        }
        (Some(call), None, None)
            if call.frontier() == input.starting_snapshot.frontier().snapshot() =>
        {
            input.starting_snapshot.clone()
        }
        (Some(_), None, None) => {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::CallSnapshotMissing,
            ));
        }
        (Some(call), Some(stored), None) => {
            if call.frontier() == input.starting_snapshot.frontier().snapshot() {
                return Err(fail(
                    input,
                    ModelCallExecutionReconstitutionFailure::CallSnapshotUnexpected,
                ));
            }
            let owner = stored.owning_session();
            let snapshot = stored.snapshot();
            let Some(current) = stored.clone().reconstitute() else {
                return Err(fail(
                    input,
                    ModelCallExecutionReconstitutionFailure::CallSnapshotMismatch,
                ));
            };
            if owner != session
                || snapshot != call.frontier()
                || current.entry_count() == input.starting_snapshot.entry_count()
                || !input.starting_snapshot.is_semantic_prefix_of(&current)
            {
                return Err(fail(
                    input,
                    ModelCallExecutionReconstitutionFailure::CallSnapshotMismatch,
                ));
            }
            current
        }
    };
    if input
        .frontier_entries
        .iter()
        .map(SemanticTranscriptEntry::reference)
        .ne(current_snapshot.ordered_entries())
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
        let accepted_input = match entry.payload() {
            SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
            | SemanticTranscriptEntryPayload::SteeringAcceptedInput { accepted_input, .. } => {
                Some(*accepted_input)
            }
            SemanticTranscriptEntryPayload::TurnFailed { .. }
            | SemanticTranscriptEntryPayload::AssistantText { .. }
            | SemanticTranscriptEntryPayload::AssistantToolUse { .. }
            | SemanticTranscriptEntryPayload::ToolExecutionResult { .. }
            | SemanticTranscriptEntryPayload::ToolDenied { .. }
            | SemanticTranscriptEntryPayload::ToolClosed { .. }
            | SemanticTranscriptEntryPayload::TurnCompleted { .. }
            | SemanticTranscriptEntryPayload::TurnCancelled { .. } => None,
        };
        if let Some(accepted_input) = accepted_input {
            if !origin_contents.contains_key(&accepted_input) {
                return Err(fail(
                    input,
                    ModelCallExecutionReconstitutionFailure::MissingOriginContent,
                ));
            }
            referenced_origins.insert(accepted_input);
        }
    }
    let pending_inputs = input
        .active_turn
        .pending_steering()
        .iter()
        .map(crate::PendingSteeringInput::accepted_input)
        .collect::<BTreeSet<_>>();
    if pending_inputs
        .iter()
        .any(|accepted_input| !origin_contents.contains_key(accepted_input))
    {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::MissingOriginContent,
        ));
    }
    if origin_contents.keys().any(|accepted_input| {
        !referenced_origins.contains(accepted_input) && !pending_inputs.contains(accepted_input)
    }) {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::UnreferencedOriginContent,
        ));
    }
    let consumed = input.active_turn.consumed_steering();
    let consumed_entries = input
        .frontier_entries
        .iter()
        .skip(input.starting_snapshot.entry_count())
        .filter(|entry| {
            matches!(
                entry.payload(),
                SemanticTranscriptEntryPayload::SteeringAcceptedInput { .. }
            )
        })
        .collect::<Vec<_>>();
    if consumed.len() != consumed_entries.len()
        || consumed
            .iter()
            .zip(consumed_entries)
            .any(|(consumed, entry)| {
                !matches!(
                    (consumed.lifecycle().disposition(), entry.payload()),
                    (
                        AcceptedInputDisposition::ConsumedAsSteering { .. },
                        SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                            accepted_input,
                            source_turn,
                        },
                    ) if *accepted_input == consumed.accepted_input()
                        && *source_turn == consumed.source_turn()
                        && *source_turn == turn
                )
            })
    {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::ConsumedSteeringMismatch,
        ));
    }
    let pinned_target = match (
        input.pinned_target,
        input.calls.first(),
        input.continuation_snapshot.as_ref(),
    ) {
        (None, None, None) => None,
        (None, None, Some(_)) | (None, Some(_), _) => {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::PinnedTargetMissing,
            ));
        }
        (Some(_), None, None) => {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::PinnedTargetUnexpected,
            ));
        }
        (Some(stored), Some(_), None) | (Some(stored), None, Some(_)) => {
            let Some(pinned) = stored.reconstitute_for_turn(turn) else {
                return Err(fail(
                    input,
                    ModelCallExecutionReconstitutionFailure::PinnedTargetTurnMismatch,
                ));
            };
            Some(pinned)
        }
        (Some(_), Some(_), Some(_)) => {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::ContinuationSnapshotUnexpected,
            ));
        }
    };
    if let Some(pinned) = pinned_target
        && input
            .targets
            .resolve(*configuration.effective().model())
            .is_ok_and(|resolution| pinned.target() != resolution.target())
    {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::CallTargetMismatch,
        ));
    }
    let current_call = if let Some(call) = input.calls.first() {
        if call.turn() != turn
            || call.attempt() != current_attempt.id()
            || call.frontier() != current_snapshot.frontier().snapshot()
        {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::CallOwnershipMismatch,
            ));
        }
        if call.selection() != *configuration.effective().model() {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::CallSelectionMismatch,
            ));
        }
        let Some(pinned) = pinned_target else {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::PinnedTargetMissing,
            ));
        };
        if call.target() != pinned.target()
            || input
                .targets
                .resolve(call.selection())
                .is_ok_and(|resolution| pinned.target() != resolution.target())
        {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::CallTargetMismatch,
            ));
        }
        match call.reconstitute(&current_snapshot, pinned) {
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
    let referenced_tool_attempts = input
        .frontier_entries
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::ToolExecutionResult { attempt } => Some(*attempt),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut tool_result_correlations = BTreeMap::new();
    for correlation in &input.tool_result_correlations {
        if tool_result_correlations
            .insert(correlation.attempt(), *correlation)
            .is_some()
        {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::ToolResultCorrelationMismatch,
            ));
        }
    }
    if referenced_tool_attempts.len() != tool_result_correlations.len()
        || referenced_tool_attempts
            .iter()
            .any(|attempt| !tool_result_correlations.contains_key(attempt))
    {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::ToolResultCorrelationMismatch,
        ));
    }
    let referenced_tool_denials = input
        .frontier_entries
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::ToolDenied { request } => Some(*request),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut tool_denial_correlations = BTreeSet::new();
    for correlation in &input.tool_denial_correlations {
        if !matches!(correlation.decision(), ToolApprovalDecision::Deny { .. })
            || !tool_denial_correlations.insert(correlation.request())
        {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::ToolDenialCorrelationMismatch,
            ));
        }
    }
    if referenced_tool_denials.len() != tool_denial_correlations.len()
        || referenced_tool_denials
            .iter()
            .any(|request| !tool_denial_correlations.contains(request))
    {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::ToolDenialCorrelationMismatch,
        ));
    }
    let running_tool_round =
        frontier_contains_tool_round(&input.starting_snapshot, &input.frontier_entries);
    let running_tool_continuation = match frontier_closes_latest_tool_round(
        &input.starting_snapshot,
        &input.frontier_entries,
        &tool_result_correlations,
        &tool_denial_correlations,
    ) {
        Ok(closed) => closed,
        Err(()) => {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::ToolResultCorrelationMismatch,
            ));
        }
    };
    let uncommitted_tool_result_projection = input
        .uncommitted_tool_result_projection
        .as_ref()
        .is_some_and(|projection| {
            projection.turn() == turn
                && projection.snapshot() == &current_snapshot
                && projection.entries().len() <= current_snapshot.entry_count()
                && projection
                    .entries()
                    .iter()
                    .map(SemanticTranscriptEntry::reference)
                    .eq(current_snapshot
                        .ordered_entries()
                        .skip(current_snapshot.entry_count() - projection.entries().len()))
        });
    if input.uncommitted_tool_result_projection.is_some() && !uncommitted_tool_result_projection {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::ContinuationSnapshotMismatch,
        ));
    }
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
            | (
                CurrentTurnAttemptState::StopRequested {
                    causes: TurnAttemptStopCauses::CancellationOnly { .. }
                },
                Some(CurrentModelCallState::CancellationRequested)
            )
    ) || matches!(
        (
            current_attempt.state(),
            current_call.as_ref().map(CurrentModelCall::state)
        ),
        (CurrentTurnAttemptState::Running, None)
            if (running_tool_round && !running_tool_continuation)
                || (running_tool_continuation && uncommitted_tool_result_projection)
    ) || matches!(
        (
            current_attempt.state(),
            current_call.as_ref().map(CurrentModelCall::state)
        ),
        (
            CurrentTurnAttemptState::Running,
            Some(CurrentModelCallState::Prepared)
        ) if running_tool_continuation
    );
    if !lifecycle_valid {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::LifecycleMismatch,
        ));
    }
    Ok(ModelCallExecution {
        active_turn: input.active_turn,
        session,
        turn,
        configuration,
        start,
        targets: input.targets,
        current_attempt,
        starting_snapshot: input.starting_snapshot,
        current_snapshot,
        frontier_entries: input.frontier_entries.into_boxed_slice(),
        origin_contents,
        pinned_target,
        current_call,
        tool_continuation_frontier: running_tool_continuation,
    })
}

fn frontier_closes_latest_tool_round(
    starting_snapshot: &ResolvedContextFrontierSnapshot,
    frontier_entries: &[SemanticTranscriptEntry],
    tool_result_correlations: &BTreeMap<crate::ToolAttemptId, ToolResultAttemptCorrelation>,
    tool_denial_correlations: &BTreeSet<crate::ToolRequestId>,
) -> Result<bool, ()> {
    let suffix = &frontier_entries[starting_snapshot.entry_count()..];
    let Some(last_tool_use) = suffix.iter().rposition(|entry| {
        matches!(
            entry.payload(),
            SemanticTranscriptEntryPayload::AssistantToolUse { .. }
        )
    }) else {
        return Ok(false);
    };
    let SemanticTranscriptEntryPayload::AssistantToolUse { producing_call, .. } =
        suffix[last_tool_use].payload()
    else {
        unreachable!("the position was selected from assistant tool-use entries");
    };
    let producing_call = *producing_call;
    let response_start = (0..=last_tool_use)
        .rev()
        .take_while(|index| assistant_entry_call(&suffix[*index]) == Some(producing_call))
        .last()
        .unwrap_or(last_tool_use);
    let response_end = (last_tool_use..suffix.len())
        .take_while(|index| assistant_entry_call(&suffix[*index]) == Some(producing_call))
        .last()
        .map_or(last_tool_use + 1, |index| index + 1);
    let requests = suffix[response_start..response_end]
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::AssistantToolUse { request, .. } => Some(*request),
            SemanticTranscriptEntryPayload::AssistantText { .. }
            | SemanticTranscriptEntryPayload::OriginAcceptedInput { .. }
            | SemanticTranscriptEntryPayload::SteeringAcceptedInput { .. }
            | SemanticTranscriptEntryPayload::TurnFailed { .. }
            | SemanticTranscriptEntryPayload::ToolExecutionResult { .. }
            | SemanticTranscriptEntryPayload::ToolDenied { .. }
            | SemanticTranscriptEntryPayload::ToolClosed { .. }
            | SemanticTranscriptEntryPayload::TurnCompleted { .. }
            | SemanticTranscriptEntryPayload::TurnCancelled { .. } => None,
        })
        .collect::<Vec<_>>();
    let Some(results_end) = response_end.checked_add(requests.len()) else {
        return Ok(false);
    };
    if requests.is_empty() || results_end > suffix.len() {
        return Ok(false);
    }
    for (entry, request) in suffix[response_end..results_end].iter().zip(&requests) {
        let valid = match entry.payload() {
            SemanticTranscriptEntryPayload::ToolExecutionResult { attempt } => {
                let Some(correlation) = tool_result_correlations.get(attempt) else {
                    return Err(());
                };
                if correlation.request() != *request
                    || correlation.producing_call() != producing_call
                {
                    return Err(());
                }
                true
            }
            SemanticTranscriptEntryPayload::ToolDenied {
                request: result_request,
            } => result_request == request && tool_denial_correlations.contains(result_request),
            SemanticTranscriptEntryPayload::ToolClosed { .. } => false,
            SemanticTranscriptEntryPayload::OriginAcceptedInput { .. }
            | SemanticTranscriptEntryPayload::SteeringAcceptedInput { .. }
            | SemanticTranscriptEntryPayload::TurnFailed { .. }
            | SemanticTranscriptEntryPayload::AssistantText { .. }
            | SemanticTranscriptEntryPayload::AssistantToolUse { .. }
            | SemanticTranscriptEntryPayload::TurnCompleted { .. }
            | SemanticTranscriptEntryPayload::TurnCancelled { .. } => false,
        };
        if !valid {
            return Ok(false);
        }
    }
    Ok(suffix[results_end..].iter().all(|entry| {
        matches!(
            entry.payload(),
            SemanticTranscriptEntryPayload::SteeringAcceptedInput { .. }
        )
    }))
}

fn assistant_entry_call(entry: &SemanticTranscriptEntry) -> Option<ModelCallId> {
    match entry.payload() {
        SemanticTranscriptEntryPayload::AssistantText { producing_call, .. }
        | SemanticTranscriptEntryPayload::AssistantToolUse { producing_call, .. } => {
            Some(*producing_call)
        }
        SemanticTranscriptEntryPayload::OriginAcceptedInput { .. }
        | SemanticTranscriptEntryPayload::SteeringAcceptedInput { .. }
        | SemanticTranscriptEntryPayload::TurnFailed { .. }
        | SemanticTranscriptEntryPayload::ToolExecutionResult { .. }
        | SemanticTranscriptEntryPayload::ToolDenied { .. }
        | SemanticTranscriptEntryPayload::ToolClosed { .. }
        | SemanticTranscriptEntryPayload::TurnCompleted { .. }
        | SemanticTranscriptEntryPayload::TurnCancelled { .. } => None,
    }
}

fn frontier_contains_tool_round(
    starting_snapshot: &ResolvedContextFrontierSnapshot,
    frontier_entries: &[SemanticTranscriptEntry],
) -> bool {
    frontier_entries
        .iter()
        .skip(starting_snapshot.entry_count())
        .any(|entry| {
            matches!(
                entry.payload(),
                SemanticTranscriptEntryPayload::AssistantToolUse { .. }
                    | SemanticTranscriptEntryPayload::ToolExecutionResult { .. }
                    | SemanticTranscriptEntryPayload::ToolDenied { .. }
                    | SemanticTranscriptEntryPayload::ToolClosed { .. }
            )
        })
}

fn reclassify_pending_steering(
    active_turn: &ActivatedAcceptedInputTurn,
    identities: &[PendingSteeringReclassificationIdentity],
) -> Result<Box<[ReclassifiedPendingSteeringTurn]>, ModelCallClosureError> {
    let pending = active_turn.pending_steering();
    if pending.len() != identities.len() {
        return Err(ModelCallClosureError::PendingSteeringReclassificationMismatch);
    }

    let mut turns = BTreeSet::new();
    let mut reclassified = Vec::with_capacity(pending.len());
    for (pending, identity) in pending.iter().zip(identities) {
        let AcceptedInputDisposition::PendingSteering { binding } =
            pending.lifecycle().disposition()
        else {
            return Err(ModelCallClosureError::PendingSteeringReclassificationMismatch);
        };
        if pending.accepted_input() != identity.accepted_input
            || binding.source_turn() != active_turn.turn()
            || identity.turn == active_turn.turn()
            || !turns.insert(identity.turn)
        {
            return Err(ModelCallClosureError::PendingSteeringReclassificationMismatch);
        }
        let accepted_input = pending
            .lifecycle()
            .clone()
            .reclassify_as_turn_origin(
                identity.turn,
                SteeringReclassificationReason::NoSafePointBeforeTerminal,
            )
            .map_err(|_| ModelCallClosureError::PendingSteeringReclassificationMismatch)?;
        reclassified.push(ReclassifiedPendingSteeringTurn {
            session: active_turn.session(),
            source_turn: active_turn.turn(),
            accepted_input,
            turn: identity.turn,
            order: AcceptedInputQueueOrder::ordinary(pending.acceptance_position()),
            binding: *binding,
            effective_configuration: active_turn.configuration().effective().clone(),
        });
    }
    Ok(reclassified.into_boxed_slice())
}

#[allow(clippy::too_many_arguments)]
fn assemble_tool_round(
    scope: ModelCallTurnScope,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    frontier_entries: Vec<SemanticTranscriptEntry>,
    response: ToolUsingAssistantResponse,
    identities: ToolRoundModelCallIdentities,
    dangerous_tool_auto_approval: DangerousToolAutoApproval,
) -> Result<ToolRoundModelCallTurn, ModelCallClosureError> {
    let ModelCallTurnScope { session, turn } = scope;
    if response.parts().len() != identities.response_parts.len() {
        return Err(ModelCallClosureError::ToolResponseIdentityMismatch);
    }
    let mut used_entries = frontier_entries
        .iter()
        .map(SemanticTranscriptEntry::identity)
        .collect::<BTreeSet<_>>();
    let mut used_requests = frontier_entries
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::AssistantToolUse { request, .. } => Some(*request),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut assistant_entries = Vec::with_capacity(response.parts().len());
    let mut requests = Vec::with_capacity(response.tool_count());
    let mut automatic_approvals = Vec::with_capacity(response.tool_count());
    let mut earliest_undecided = None;
    let mut tool_ordinal = 0usize;

    for (part, identity) in response.parts().iter().zip(identities.response_parts) {
        let entry = match (part, identity) {
            (AssistantResponsePart::Text(value), ToolResponsePartIdentity::Text { entry }) => {
                if !used_entries.insert(entry) {
                    return Err(ModelCallClosureError::FrontierDerivationFailed);
                }
                SemanticTranscriptEntry::from_validated_parts(
                    entry,
                    session,
                    SemanticTranscriptEntryPayload::AssistantText {
                        producing_call: call.id(),
                        value: value.clone(),
                    },
                )
            }
            (
                AssistantResponsePart::ToolCall(proposal),
                ToolResponsePartIdentity::ToolCall {
                    entry,
                    request,
                    approval,
                },
            ) => {
                if !used_entries.insert(entry) || !used_requests.insert(request) {
                    return Err(ModelCallClosureError::FrontierDerivationFailed);
                }
                let approval_matches = match dangerous_tool_auto_approval {
                    DangerousToolAutoApproval::ApproveAll => {
                        approval == InitialToolApproval::SessionBlanket
                    }
                    DangerousToolAutoApproval::Disabled => {
                        approval != InitialToolApproval::SessionBlanket
                    }
                };
                if !approval_matches {
                    return Err(ModelCallClosureError::InitialToolApprovalMismatch);
                }
                let ordinal = ToolRequestOrdinal::try_from_usize(tool_ordinal)
                    .ok_or(ModelCallClosureError::ToolRequestOrdinalOverflow)?;
                tool_ordinal += 1;
                let request_record = ToolRequest::from_model_proposal(
                    request,
                    session,
                    turn,
                    call.id(),
                    ordinal,
                    proposal.clone(),
                );
                match approval.resolution(request) {
                    Some(resolution) => automatic_approvals.push(resolution),
                    None => {
                        earliest_undecided.get_or_insert(request);
                    }
                }
                requests.push(request_record);
                SemanticTranscriptEntry::from_validated_parts(
                    entry,
                    session,
                    SemanticTranscriptEntryPayload::AssistantToolUse {
                        producing_call: call.id(),
                        request,
                    },
                )
            }
            _ => return Err(ModelCallClosureError::ToolResponseIdentityMismatch),
        };
        assistant_entries.push(entry);
    }

    let next_phase = match (earliest_undecided, identities.continuation_attempt) {
        (Some(request), None) => ActiveTurnPhase::AwaitingApproval { request },
        (None, Some(continuation)) if continuation != attempt.id() => ActiveTurnPhase::Running {
            current_attempt: CurrentTurnAttempt::prepared(continuation),
        },
        _ => return Err(ModelCallClosureError::ContinuationAttemptIdentityMismatch),
    };
    let source = ResolvedContextFrontierSnapshot::try_from_candidate(
        session,
        call.frontier().snapshot(),
        frontier_entries
            .iter()
            .map(SemanticTranscriptEntry::reference)
            .collect(),
    )
    .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    let yielded_snapshot = source
        .derive_appending_candidate(
            identities.yielded_frontier,
            assistant_entries
                .iter()
                .map(SemanticTranscriptEntry::reference)
                .collect(),
        )
        .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;

    Ok(ToolRoundModelCallTurn {
        session,
        turn,
        call,
        attempt,
        assistant_entries: assistant_entries.into_boxed_slice(),
        requests: requests.into_boxed_slice(),
        automatic_approvals: automatic_approvals.into_boxed_slice(),
        yielded_snapshot,
        next_phase,
    })
}

#[allow(clippy::too_many_arguments)]
fn assemble_stopped_tool_round(
    scope: ModelCallTurnScope,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    frontier_entries: Vec<SemanticTranscriptEntry>,
    response: ToolUsingAssistantResponse,
    proof: AppliedInterruptProof,
    identities: StoppedToolRoundModelCallIdentities,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
) -> Result<CancelledToolRoundModelCallTurn, ModelCallClosureError> {
    let ModelCallTurnScope { session, turn } = scope;
    if proof.predecessor() != turn || response.parts().len() != identities.response_parts.len() {
        return Err(ModelCallClosureError::ToolResponseIdentityMismatch);
    }
    let mut used_entries = frontier_entries
        .iter()
        .map(SemanticTranscriptEntry::identity)
        .collect::<BTreeSet<_>>();
    if !used_entries.insert(identities.cancellation_entry) {
        return Err(ModelCallClosureError::FrontierDerivationFailed);
    }
    let mut used_requests = frontier_entries
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::AssistantToolUse { request, .. } => Some(*request),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut assistant_entries = Vec::with_capacity(response.parts().len());
    let mut requests = Vec::with_capacity(response.tool_count());
    let mut closed_result_entries = Vec::with_capacity(response.tool_count());
    let mut tool_ordinal = 0usize;

    for (part, identity) in response.parts().iter().zip(identities.response_parts) {
        let entry = match (part, identity) {
            (
                AssistantResponsePart::Text(value),
                StoppedToolResponsePartIdentity::Text { entry },
            ) => {
                if !used_entries.insert(entry) {
                    return Err(ModelCallClosureError::FrontierDerivationFailed);
                }
                SemanticTranscriptEntry::from_validated_parts(
                    entry,
                    session,
                    SemanticTranscriptEntryPayload::AssistantText {
                        producing_call: call.id(),
                        value: value.clone(),
                    },
                )
            }
            (
                AssistantResponsePart::ToolCall(proposal),
                StoppedToolResponsePartIdentity::ToolCall {
                    entry,
                    request,
                    closed_result_entry,
                },
            ) => {
                if !used_entries.insert(entry)
                    || !used_entries.insert(closed_result_entry)
                    || !used_requests.insert(request)
                {
                    return Err(ModelCallClosureError::FrontierDerivationFailed);
                }
                let ordinal = ToolRequestOrdinal::try_from_usize(tool_ordinal)
                    .ok_or(ModelCallClosureError::ToolRequestOrdinalOverflow)?;
                tool_ordinal += 1;
                requests.push(ToolRequest::from_model_proposal(
                    request,
                    session,
                    turn,
                    call.id(),
                    ordinal,
                    proposal.clone(),
                ));
                closed_result_entries.push(SemanticTranscriptEntry::from_validated_parts(
                    closed_result_entry,
                    session,
                    SemanticTranscriptEntryPayload::ToolClosed { request },
                ));
                SemanticTranscriptEntry::from_validated_parts(
                    entry,
                    session,
                    SemanticTranscriptEntryPayload::AssistantToolUse {
                        producing_call: call.id(),
                        request,
                    },
                )
            }
            _ => return Err(ModelCallClosureError::ToolResponseIdentityMismatch),
        };
        assistant_entries.push(entry);
    }
    let cancellation_entry = SemanticTranscriptEntry::from_validated_parts(
        identities.cancellation_entry,
        session,
        SemanticTranscriptEntryPayload::TurnCancelled { turn },
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
        .chain(
            closed_result_entries
                .iter()
                .map(SemanticTranscriptEntry::reference),
        )
        .chain([cancellation_entry.reference()])
        .collect();
    let terminal_snapshot = source
        .derive_appending_candidate(identities.terminal_frontier, appended)
        .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    Ok(CancelledToolRoundModelCallTurn {
        session,
        turn,
        call,
        attempt,
        disposition: TurnDisposition::Cancelled { cause: proof },
        assistant_entries: assistant_entries.into_boxed_slice(),
        requests: requests.into_boxed_slice(),
        closed_result_entries: closed_result_entries.into_boxed_slice(),
        cancellation_entry,
        terminal_snapshot,
        reclassified_pending_steering,
    })
}

pub(crate) fn apply_interrupt_to_recovery_wait(
    active_turn: ActivatedAcceptedInputTurn,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    source_snapshot: ResolvedContextFrontierSnapshot,
    interrupt: AppliedInterruptCommandResult,
    identities: AmbiguousModelCallTurnIdentities,
) -> Result<ReconciliationRequiredModelCallTurn, ModelCallClosureError> {
    let proof = interrupt.proof();
    let ActiveTurnPhase::AwaitingRecoveryDecision {
        ambiguous_operations,
        applied_interrupt: None,
    } = active_turn.phase()
    else {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    };
    if interrupt.session() != active_turn.session()
        || proof.predecessor() != active_turn.turn()
        || interrupt.successor() == active_turn.turn()
        || interrupt.successor_order().priority()
            != (crate::AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                predecessor: active_turn.turn(),
            })
        || ambiguous_operations.operation_count() != 1
        || !ambiguous_operations.contains(crate::IssuedOperationRef::ModelCall(call.id()))
        || call.turn() != active_turn.turn()
        || call.attempt() != attempt.id()
        || call.disposition() != ModelCallDisposition::Ambiguous
        || call.frontier() != source_snapshot.frontier()
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let reclassified_pending_steering =
        reclassify_pending_steering(&active_turn, &identities.pending_steering_reclassifications)?;
    let terminal_snapshot = source_snapshot
        .derive_appending_candidate(identities.terminal_frontier, Vec::new())
        .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    if !matches!(
        attempt.end(),
        AttemptEnd::WithoutStop {
            disposition: UnstoppedAttemptDisposition::Ambiguous | UnstoppedAttemptDisposition::Lost,
        }
    ) {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    }
    let marker =
        ReconciliationMarker::from_interrupt_ambiguity(ambiguous_operations.clone(), proof);
    Ok(ReconciliationRequiredModelCallTurn {
        session: active_turn.session(),
        turn: active_turn.turn(),
        call,
        attempt,
        disposition: TurnDisposition::ReconciliationRequired { marker },
        terminal_snapshot,
        reclassified_pending_steering,
    })
}

pub(crate) fn apply_interrupt_to_tool_recovery_wait(
    active_turn: ActivatedAcceptedInputTurn,
    wait: AwaitingToolRecovery,
    tool_attempt: EndedToolAttempt,
    attempt: EndedTurnAttempt,
    result_projection: PreparedToolResultProjection,
    interrupt: AppliedInterruptCommandResult,
    identities: AmbiguousModelCallTurnIdentities,
) -> Result<ReconciliationRequiredToolTurn, ModelCallClosureError> {
    let proof = interrupt.proof();
    let ActiveTurnPhase::AwaitingRecoveryDecision {
        ambiguous_operations,
        applied_interrupt,
    } = active_turn.phase()
    else {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    };
    let attempt_end_matches = match attempt.end() {
        AttemptEnd::WithoutStop { disposition } => {
            applied_interrupt.is_none()
                && matches!(
                    disposition,
                    UnstoppedAttemptDisposition::Ambiguous | UnstoppedAttemptDisposition::Lost
                )
        }
        AttemptEnd::AfterCancellation { cause, disposition } => {
            *cause == proof
                && applied_interrupt == &Some(proof)
                && matches!(
                    disposition,
                    CancellationStopDisposition::Ambiguous | CancellationStopDisposition::Lost
                )
        }
        AttemptEnd::AfterFatalMismatch { .. } => false,
    };
    if interrupt.session() != active_turn.session()
        || proof.predecessor() != active_turn.turn()
        || interrupt.successor() == active_turn.turn()
        || interrupt.successor_order().priority()
            != (crate::AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                predecessor: active_turn.turn(),
            })
        || ambiguous_operations.operation_count() != 1
        || !ambiguous_operations.contains(crate::IssuedOperationRef::ToolAttempt(wait.attempt()))
        || wait.session() != active_turn.session()
        || wait.turn() != active_turn.turn()
        || wait.producing_call() != result_projection.producing_call()
        || wait.issuing_attempt() != attempt.id()
        || wait.attempt() != tool_attempt.attempt()
        || result_projection.turn() != active_turn.turn()
        || wait.yielded_frontier() != result_projection.source_frontier()
        || result_projection.snapshot().frontier().owning_session() != active_turn.session()
        || result_projection.snapshot().frontier().snapshot() != identities.terminal_frontier
        || !result_projection.entries().iter().any(|entry| {
            entry.payload()
                == &SemanticTranscriptEntryPayload::ToolClosed {
                    request: tool_attempt.request(),
                }
        })
        || tool_attempt.session() != active_turn.session()
        || tool_attempt.turn() != active_turn.turn()
        || tool_attempt.issuing_attempt() != attempt.id()
        || tool_attempt.end() != &crate::ToolAttemptEnd::Ambiguous
        || !attempt_end_matches
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let reclassified_pending_steering =
        reclassify_pending_steering(&active_turn, &identities.pending_steering_reclassifications)?;
    let (tool_result_entries, terminal_snapshot) = result_projection.into_parts();
    let marker =
        ReconciliationMarker::from_interrupt_ambiguity(ambiguous_operations.clone(), proof);
    Ok(ReconciliationRequiredToolTurn {
        session: active_turn.session(),
        turn: active_turn.turn(),
        tool_attempt,
        attempt,
        disposition: TurnDisposition::ReconciliationRequired { marker },
        tool_result_entries,
        terminal_snapshot,
        reclassified_pending_steering,
    })
}

fn complete_turn(
    scope: ModelCallTurnScope,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    frontier_entries: Vec<SemanticTranscriptEntry>,
    assistant_text: Vec<AssistantText>,
    identities: CompletedModelCallIdentities,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
) -> Result<CompletedModelCallTurn, ModelCallClosureError> {
    let ModelCallTurnScope { session, turn } = scope;
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
        reclassified_pending_steering,
    })
}

fn close_failed_turn(
    scope: ModelCallTurnScope,
    attempt: CurrentTurnAttempt,
    call: Option<EndedModelCall>,
    source: ResolvedContextFrontierSnapshot,
    identities: FailedModelCallTurnIdentities,
    attempt_disposition: UnstoppedAttemptDisposition,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
) -> Result<FailedModelCallTurn, ModelCallClosureError> {
    let ended_attempt = attempt
        .end_without_stop(attempt_disposition)
        .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
    assemble_failed_turn(
        scope,
        ended_attempt,
        call,
        source,
        identities,
        reclassified_pending_steering,
    )
}

fn close_failed_turn_after_cancellation(
    scope: ModelCallTurnScope,
    attempt: CurrentTurnAttempt,
    call: EndedModelCall,
    source: ResolvedContextFrontierSnapshot,
    proof: AppliedInterruptProof,
    identities: FailedModelCallTurnIdentities,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
) -> Result<FailedModelCallTurn, ModelCallClosureError> {
    let ended_attempt = attempt
        .end_after_cancellation(proof, CancellationStopDisposition::KnownFailure)
        .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
    assemble_failed_turn(
        scope,
        ended_attempt,
        Some(call),
        source,
        identities,
        reclassified_pending_steering,
    )
}

fn assemble_failed_turn(
    scope: ModelCallTurnScope,
    ended_attempt: EndedTurnAttempt,
    call: Option<EndedModelCall>,
    source: ResolvedContextFrontierSnapshot,
    identities: FailedModelCallTurnIdentities,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
) -> Result<FailedModelCallTurn, ModelCallClosureError> {
    let ModelCallTurnScope { session, turn } = scope;
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
        reclassified_pending_steering,
    })
}

fn close_cancelled_turn(
    scope: ModelCallTurnScope,
    attempt: CurrentTurnAttempt,
    call: Option<EndedModelCall>,
    source: ResolvedContextFrontierSnapshot,
    proof: AppliedInterruptProof,
    identities: CancelledModelCallTurnIdentities,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
) -> Result<CancelledModelCallTurn, ModelCallClosureError> {
    let ModelCallTurnScope { session, turn } = scope;
    if proof.predecessor() != turn {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let ended_attempt = attempt
        .end_after_cancellation(proof, CancellationStopDisposition::Cancelled)
        .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
    let cancellation_entry = SemanticTranscriptEntry::from_validated_parts(
        identities.cancellation_entry,
        session,
        SemanticTranscriptEntryPayload::TurnCancelled { turn },
    );
    let terminal_snapshot = source
        .derive_appending_candidate(
            identities.terminal_frontier,
            vec![cancellation_entry.reference()],
        )
        .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    Ok(CancelledModelCallTurn {
        session,
        turn,
        call,
        attempt: ended_attempt,
        disposition: TurnDisposition::Cancelled { cause: proof },
        tool_result_entries: Box::new([]),
        cancellation_entry,
        terminal_snapshot,
        reclassified_pending_steering,
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
        NormalizedToolArguments, PerInputConfigurationChoices, SemanticTranscriptEntryRef, Session,
        SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionCreationCause,
        SessionCreationProvenance, SessionReconstitutionInput, ToolApprovalDecision,
        ToolApprovalResolutionReconstitutionInput, ToolBatchPhaseReconstitutionInput,
        ToolBatchReconstitutionInput, ToolDecisionSource, ToolName, ToolRequestOrdinal,
        ToolRequestReconstitutionInput, TranscriptAncestry,
        test_support::{
            accepted_input_id, context_frontier_id, direct, model_call_id, provider_model_identity,
            semantic_transcript_entry_id, session_id, tool_attempt_id, tool_request_id,
            turn_attempt_id, turn_id,
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
            turn,
            targets(),
            snapshot,
            vec![origin],
            vec![ModelCallOriginContent::from_validated_parts(
                accepted_input_id(4),
                UserContent::try_text(String::from("hello")).expect("test content is valid"),
            )],
            None,
            Vec::new(),
        )
        .reconstitute()
        .expect("activation facts reconstruct live execution")
    }

    fn targets() -> ModelTargetCatalog {
        ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
            direct(2),
            ResolvedProviderTarget::naming(provider_model_identity(8)),
        )])
        .expect("one definition is unique")
    }

    fn prepared_execution() -> ModelCallExecution {
        let initial = active_execution();
        let prepared = initial
            .clone()
            .prepare_initial_call(model_call_id(9))
            .expect("initial prepared checkpoint is valid");
        ModelCallExecutionReconstitutionInput::new(
            initial.active_turn.clone(),
            initial.targets.clone(),
            initial.starting_snapshot.clone(),
            initial.frontier_entries.to_vec(),
            initial
                .origin_contents
                .iter()
                .map(|(accepted_input, content)| {
                    ModelCallOriginContent::from_validated_parts(*accepted_input, content.clone())
                })
                .collect(),
            Some(PinnedProviderTargetReconstitutionInput::new(
                prepared.call().turn(),
                prepared.call().target(),
            )),
            vec![ModelCallReconstitutionInput::new(
                prepared.call().id(),
                prepared.call().turn(),
                prepared.call().attempt(),
                prepared.call().selection(),
                prepared.call().target(),
                prepared.call().frontier().snapshot(),
                ModelCallReconstitutionState::Prepared,
            )],
        )
        .reconstitute()
        .expect("prepared facts reconstruct")
    }

    fn prepared_execution_consuming_steering() -> ModelCallExecution {
        let mut initial = active_execution();
        let accepted_input = accepted_input_id(20);
        let acceptance_position = crate::SessionInputPosition::try_from_u64(2)
            .expect("the steering position is positive");
        initial.active_turn = initial.active_turn.with_pending_steering_for_test(
            vec![(accepted_input, acceptance_position)].into_boxed_slice(),
        );
        initial.origin_contents.insert(
            accepted_input,
            UserContent::try_text(String::from("steer")).expect("steering content is valid"),
        );
        let active_turn = initial.active_turn.clone();
        let targets = initial.targets.clone();
        let starting_snapshot = initial.starting_snapshot.clone();
        let mut frontier_entries = initial.frontier_entries.to_vec();
        let origin_contents = initial
            .origin_contents
            .iter()
            .map(|(accepted_input, content)| {
                ModelCallOriginContent::from_validated_parts(*accepted_input, content.clone())
            })
            .collect();
        let call_id = model_call_id(9);
        let prepared = initial
            .prepare_initial_call_consuming_steering(
                call_id,
                vec![semantic_transcript_entry_id(22)],
                Some(context_frontier_id(24)),
            )
            .expect("steering may be consumed by a prepared call");
        frontier_entries.extend(
            prepared
                .consumed_steering()
                .iter()
                .map(|consumed| consumed.semantic_entry().clone()),
        );
        let call = prepared.call();
        let call_snapshot = prepared
            .steering_snapshot()
            .expect("steering creates a call snapshot");
        ModelCallExecutionReconstitutionInput::new(
            active_turn.with_consumed_steering_for_test(
                vec![(accepted_input, acceptance_position, call_id)].into_boxed_slice(),
            ),
            targets,
            starting_snapshot,
            frontier_entries,
            origin_contents,
            Some(PinnedProviderTargetReconstitutionInput::new(
                call.turn(),
                call.target(),
            )),
            vec![ModelCallReconstitutionInput::new(
                call.id(),
                call.turn(),
                call.attempt(),
                call.selection(),
                call.target(),
                call.frontier().snapshot(),
                ModelCallReconstitutionState::Prepared,
            )],
        )
        .with_call_snapshot(ResolvedContextFrontierReconstitutionInput::new(
            session_id(1),
            call_snapshot.frontier().snapshot(),
            call_snapshot.ordered_entries().collect(),
        ))
        .reconstitute()
        .expect("a steering-consuming prepared call reconstructs")
    }

    fn in_flight_execution() -> ModelCallExecution {
        let prepared = prepared_execution();
        let authorized = prepared
            .clone()
            .authorize_send()
            .expect("prepared execution may authorize send");
        ModelCallExecutionReconstitutionInput::new(
            prepared
                .active_turn
                .with_phase_for_test(ActiveTurnPhase::Running {
                    current_attempt: authorized.attempt.clone(),
                }),
            prepared.targets.clone(),
            prepared.starting_snapshot.clone(),
            prepared.frontier_entries.to_vec(),
            prepared
                .origin_contents
                .iter()
                .map(|(accepted_input, content)| {
                    ModelCallOriginContent::from_validated_parts(*accepted_input, content.clone())
                })
                .collect(),
            Some(PinnedProviderTargetReconstitutionInput::new(
                authorized.call.turn(),
                authorized.call.target(),
            )),
            vec![ModelCallReconstitutionInput::new(
                authorized.call.id(),
                authorized.call.turn(),
                authorized.call.attempt(),
                authorized.call.selection(),
                authorized.call.target(),
                authorized.call.frontier().snapshot(),
                ModelCallReconstitutionState::InFlight,
            )],
        )
        .reconstitute()
        .expect("in-flight facts reconstruct")
    }

    fn reconstitution_input_with_calls(
        execution: &ModelCallExecution,
        calls: Vec<ModelCallReconstitutionInput>,
    ) -> ModelCallExecutionReconstitutionInput {
        let pinned_target = execution
            .current_call()
            .map(|call| PinnedProviderTargetReconstitutionInput::new(call.turn(), call.target()));
        ModelCallExecutionReconstitutionInput::new(
            execution
                .active_turn
                .with_phase_for_test(ActiveTurnPhase::Running {
                    current_attempt: execution.current_attempt.clone(),
                }),
            execution.targets.clone(),
            execution.starting_snapshot.clone(),
            execution.frontier_entries.to_vec(),
            execution
                .origin_contents
                .iter()
                .map(|(accepted_input, content)| {
                    ModelCallOriginContent::from_validated_parts(*accepted_input, content.clone())
                })
                .collect(),
            pinned_target,
            calls,
        )
    }

    fn correlated_observation(
        execution: &ModelCallExecution,
        observation: ModelCallTerminalObservation,
    ) -> CorrelatedModelCallTerminalObservation {
        let call = execution
            .current_call()
            .expect("a correlated test observation requires one live call");
        CorrelatedModelCallTerminalObservation {
            correlation: IssuedModelCallCorrelation {
                session: execution.session(),
                turn: execution.turn(),
                attempt: execution.current_attempt().id(),
                call: call.id(),
                target: call.target(),
                frontier: call.frontier().snapshot(),
            },
            observation,
        }
    }

    fn tool_proposal(name: &str, arguments: &str) -> crate::ToolCallProposal {
        crate::ToolCallProposal::new(
            ToolName::try_new(name.to_owned()).expect("test tool names are canonical"),
            NormalizedToolArguments::try_from_provider_text(arguments.to_owned())
                .expect("test arguments fit the admission bound"),
        )
    }

    fn batch_request(id: u128, execution: &ModelCallExecution) -> ToolRequest {
        ToolRequestReconstitutionInput::new(
            tool_request_id(id),
            execution.session(),
            execution.turn(),
            model_call_id(40),
            ToolRequestOrdinal::from_u32(0),
            ToolName::try_new(String::from("fixture_tool")).expect("the tool name is valid"),
            NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("the fixture arguments are canonical"),
        )
        .into_request()
    }

    fn denied_approval(request: ToolRequestId) -> ToolApprovalResolution {
        ToolApprovalResolutionReconstitutionInput::new(
            request,
            ToolApprovalDecision::Deny { reason: None },
            ToolDecisionSource::OwnerCommand,
        )
        .reconstitute()
        .expect("the denial fixture is implemented")
    }

    fn with_pending_steering(
        mut execution: ModelCallExecution,
        pending: AcceptedInputId,
    ) -> ModelCallExecution {
        execution.active_turn = execution.active_turn.with_pending_steering_for_test(
            vec![(
                pending,
                crate::SessionInputPosition::try_from_u64(2)
                    .expect("the test steering position is positive"),
            )]
            .into_boxed_slice(),
        );
        execution
    }

    fn one_reclassification(
        pending: AcceptedInputId,
        turn: TurnId,
    ) -> Vec<PendingSteeringReclassificationIdentity> {
        vec![PendingSteeringReclassificationIdentity::new(pending, turn)]
    }

    fn applied_interrupt(execution: &ModelCallExecution) -> AppliedInterruptCommandResult {
        AppliedInterruptCommandResult::from_correlated_submit(
            crate::test_support::command_id(30),
            execution.session(),
            execution.turn(),
            accepted_input_id(31),
            turn_id(32),
            AcceptedInputQueueOrder::interrupt_immediately_after(
                crate::SessionInputPosition::try_from_u64(2)
                    .expect("the interrupt acceptance position is positive"),
                execution.turn(),
            ),
        )
        .expect("the fixture interrupt is exactly correlated")
    }

    fn stop_requested_execution(
        execution: ModelCallExecution,
    ) -> (ModelCallExecution, AppliedInterruptCommandResult) {
        let interrupt = applied_interrupt(&execution);
        let outcome = execution
            .clone()
            .apply_interrupt(
                interrupt,
                CancelledModelCallTurnIdentities::new(
                    semantic_transcript_entry_id(33),
                    context_frontier_id(34),
                ),
            )
            .expect("an issued call accepts the matching interrupt");
        let ModelCallInterruptOutcome::CancellationRequested(stopped) = outcome else {
            panic!("issued work requests physical cancellation");
        };
        let mut reloaded = execution;
        reloaded.current_attempt = stopped.attempt().clone();
        reloaded.current_call = Some(stopped.call().clone());
        reloaded.active_turn = reloaded
            .active_turn
            .with_phase_for_test(ActiveTurnPhase::Running {
                current_attempt: stopped.attempt().clone(),
            });
        (reloaded, interrupt)
    }

    fn assert_one_reclassified_turn(
        reclassified: &[ReclassifiedPendingSteeringTurn],
        pending: AcceptedInputId,
        source_turn: TurnId,
        successor: TurnId,
    ) {
        assert_eq!(reclassified.len(), 1);
        let reclassified = &reclassified[0];
        assert_eq!(reclassified.session(), session_id(1));
        assert_eq!(reclassified.source_turn(), source_turn);
        assert_eq!(reclassified.accepted_input().id(), pending);
        assert_eq!(reclassified.turn(), successor);
        assert_eq!(
            reclassified.accepted_input().disposition(),
            &AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                turn: successor,
                reason: SteeringReclassificationReason::NoSafePointBeforeTerminal,
            }
        );
        assert_eq!(reclassified.binding().source_turn(), source_turn);
        assert_eq!(
            reclassified.order(),
            AcceptedInputQueueOrder::ordinary(
                crate::SessionInputPosition::try_from_u64(2)
                    .expect("the test steering position is positive")
            )
        );
        assert_eq!(
            reclassified.effective_configuration().model(),
            &FrozenModelSelection::Direct(direct(2))
        );
    }

    /// S02 / INV-005 / INV-015: a complete frontier read must preserve exact
    /// semantic order, not merely the same entry membership.
    #[test]
    fn s02_inv005_inv015_reconstitution_rejects_reordered_frontier_entries() {
        let execution = active_execution();
        let first = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(20),
            execution.session,
            SemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: accepted_input_id(21),
            },
        );
        let second = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(22),
            execution.session,
            SemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: accepted_input_id(23),
            },
        );
        let snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
            execution.session,
            context_frontier_id(24),
            vec![first.reference(), second.reference()],
        )
        .expect("ordered test frontier is valid");
        let start = AcceptedInputTurnStart::from_validated_eligibility(
            crate::AcceptedInputStartingLineage::FirstInSession,
            snapshot.frontier(),
        );
        let input = ModelCallExecutionReconstitutionInput::new(
            execution.active_turn.with_start_for_test(start),
            execution.targets.clone(),
            snapshot,
            vec![second, first],
            vec![
                ModelCallOriginContent::from_validated_parts(
                    accepted_input_id(21),
                    UserContent::try_text(String::from("first")).expect("valid text"),
                ),
                ModelCallOriginContent::from_validated_parts(
                    accepted_input_id(23),
                    UserContent::try_text(String::from("second")).expect("valid text"),
                ),
            ],
            None,
            Vec::new(),
        );

        let error = input
            .reconstitute()
            .expect_err("same membership in another order is not the stored frontier");
        assert_eq!(
            error.failure(),
            ModelCallExecutionReconstitutionFailure::FrontierEntryMismatch
        );
    }

    /// S02 / INV-009 / INV-015: an execution snapshot must be the exact
    /// eligibility-fixed turn start, not another same-content frontier.
    #[test]
    fn s02_inv009_inv015_reconstitution_rejects_nonstarting_snapshot() {
        let execution = active_execution();
        let other_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
            execution.session,
            context_frontier_id(25),
            execution.starting_snapshot.ordered_entries().collect(),
        )
        .expect("same-content test snapshot is valid");
        let input = ModelCallExecutionReconstitutionInput::new(
            execution.active_turn.clone(),
            execution.targets.clone(),
            other_snapshot,
            execution.frontier_entries.to_vec(),
            execution
                .origin_contents
                .iter()
                .map(|(accepted_input, content)| {
                    ModelCallOriginContent::from_validated_parts(*accepted_input, content.clone())
                })
                .collect(),
            None,
            Vec::new(),
        );

        let error = input
            .reconstitute()
            .expect_err("a same-content snapshot is not the fixed starting frontier");
        assert_eq!(
            error.failure(),
            ModelCallExecutionReconstitutionFailure::StartingSnapshotMismatch
        );
    }

    /// S02 / S11 / INV-005 / INV-014 / INV-015 / INV-036: a fresh
    /// continuation attempt admits its call-free result frontier only inside
    /// the transaction that will insert the prepared continuation call.
    #[test]
    fn s02_s11_inv005_inv014_inv015_inv036_continuation_reconstitutes_exact_frontier_and_pin() {
        let initial = active_execution();
        let request = tool_request_id(30);
        let assistant_tool_use = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(31),
            initial.session,
            SemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call: model_call_id(32),
                request,
            },
        );
        let denied = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(33),
            initial.session,
            SemanticTranscriptEntryPayload::ToolDenied { request },
        );
        let yielded = initial
            .starting_snapshot
            .derive_appending_candidate(
                context_frontier_id(34),
                vec![assistant_tool_use.reference()],
            )
            .expect("tool proposal preserves the starting prefix");
        let continuation = yielded
            .derive_appending_candidate(context_frontier_id(37), vec![denied.reference()])
            .expect("tool denial extends the yielded frontier");
        let projection = PreparedToolResultProjection::from_validated_parts(
            yielded.frontier().snapshot(),
            initial.turn,
            model_call_id(32),
            vec![denied.clone()],
            continuation.clone(),
        );
        let pinned = PinnedProviderTargetReconstitutionInput::new(
            initial.turn,
            ResolvedProviderTarget::naming(provider_model_identity(8)),
        );
        let input = ModelCallExecutionReconstitutionInput::new(
            initial
                .active_turn
                .with_phase_for_test(ActiveTurnPhase::Running {
                    current_attempt: CurrentTurnAttempt::prepared(turn_attempt_id(35))
                        .begin_running()
                        .expect("tool dispatch starts the continuation tenure"),
                }),
            initial.targets,
            initial.starting_snapshot,
            vec![
                initial.frontier_entries[0].clone(),
                assistant_tool_use,
                denied,
            ],
            initial
                .origin_contents
                .into_iter()
                .map(|(accepted_input, content)| {
                    ModelCallOriginContent::from_validated_parts(accepted_input, content)
                })
                .collect(),
            Some(pinned),
            Vec::new(),
        )
        .with_continuation_snapshot(ResolvedContextFrontierReconstitutionInput::new(
            continuation.frontier().owning_session(),
            continuation.frontier().snapshot(),
            continuation.ordered_entries().collect(),
        ))
        .with_tool_denial_correlations(vec![denied_approval(request)]);
        let mut missing_denial = input.clone();
        missing_denial.tool_denial_correlations.clear();
        assert_eq!(
            missing_denial
                .reconstitute()
                .expect_err("a denial entry requires its exact durable resolution")
                .failure(),
            ModelCallExecutionReconstitutionFailure::ToolDenialCorrelationMismatch
        );
        let mut approved_instead = input.clone();
        approved_instead.tool_denial_correlations = vec![
            ToolApprovalResolutionReconstitutionInput::new(
                request,
                ToolApprovalDecision::Approve,
                ToolDecisionSource::OwnerCommand,
            )
            .reconstitute()
            .expect("the mismatching approval fixture is valid"),
        ];
        assert_eq!(
            approved_instead
                .reconstitute()
                .expect_err("approval authority cannot back a denial entry")
                .failure(),
            ModelCallExecutionReconstitutionFailure::ToolDenialCorrelationMismatch
        );
        assert_eq!(
            input
                .clone()
                .reconstitute()
                .expect_err("a durably visible resolved frontier requires its prepared call")
                .failure(),
            ModelCallExecutionReconstitutionFailure::LifecycleMismatch
        );
        let input = input.with_uncommitted_tool_result_projection(projection);
        let resumed = input
            .clone()
            .reconstitute()
            .expect("the exact transaction-local projection reconstructs");
        let prepared = resumed
            .prepare_initial_call(model_call_id(36))
            .expect("the continuation attempt prepares its next call");

        assert_eq!(prepared.call().frontier(), continuation.frontier());
        assert_eq!(
            prepared.call().target(),
            ResolvedProviderTarget::naming(provider_model_identity(8))
        );
        assert!(prepared.steering_snapshot().is_none());

        let mut prepared_input = input;
        prepared_input.call_snapshot = prepared_input.continuation_snapshot.take();
        prepared_input.uncommitted_tool_result_projection = None;
        prepared_input.calls = vec![ModelCallReconstitutionInput::new(
            prepared.call().id(),
            prepared.turn(),
            prepared.attempt(),
            prepared.call().selection(),
            prepared.call().target(),
            prepared.call().frontier().snapshot(),
            ModelCallReconstitutionState::Prepared,
        )];
        let reloaded = prepared_input
            .reconstitute()
            .expect("a running tool tenure may own its prepared continuation call");
        assert_eq!(
            reloaded
                .resume_prepared_call()
                .expect("the prepared continuation resumes")
                .call()
                .id(),
            prepared.call().id()
        );
        let authorized = reloaded
            .authorize_send()
            .expect("send authorization keeps the existing running tenure");
        assert_eq!(
            authorized.attempt().state(),
            &CurrentTurnAttemptState::Running
        );
        assert_eq!(authorized.call().state(), CurrentModelCallState::InFlight);
    }

    /// S10 / INV-004 / INV-005: each continuation result must name the
    /// physical attempt that executed its exact request in the producing
    /// model-call batch.
    #[test]
    fn s10_inv004_inv005_continuation_rejects_duplicate_attempt_for_two_requests() {
        let initial = active_execution();
        let producing_call = model_call_id(70);
        let first_request = tool_request_id(71);
        let second_request = tool_request_id(72);
        let first_use = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(73),
            initial.session,
            SemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request: first_request,
            },
        );
        let second_use = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(74),
            initial.session,
            SemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request: second_request,
            },
        );
        let shared_attempt = tool_attempt_id(75);
        let first_result = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(76),
            initial.session,
            SemanticTranscriptEntryPayload::ToolExecutionResult {
                attempt: shared_attempt,
            },
        );
        let second_result = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(77),
            initial.session,
            SemanticTranscriptEntryPayload::ToolExecutionResult {
                attempt: shared_attempt,
            },
        );
        let continuation = initial
            .starting_snapshot
            .derive_appending_candidate(
                context_frontier_id(78),
                vec![
                    first_use.reference(),
                    second_use.reference(),
                    first_result.reference(),
                    second_result.reference(),
                ],
            )
            .expect("the malformed candidate still preserves the starting prefix");
        let input = ModelCallExecutionReconstitutionInput::new(
            initial
                .active_turn
                .with_phase_for_test(ActiveTurnPhase::Running {
                    current_attempt: CurrentTurnAttempt::prepared(turn_attempt_id(79))
                        .begin_running()
                        .expect("the tool tenure is running"),
                }),
            initial.targets,
            initial.starting_snapshot,
            vec![
                initial.frontier_entries[0].clone(),
                first_use,
                second_use,
                first_result,
                second_result,
            ],
            initial
                .origin_contents
                .into_iter()
                .map(|(accepted_input, content)| {
                    ModelCallOriginContent::from_validated_parts(accepted_input, content)
                })
                .collect(),
            Some(PinnedProviderTargetReconstitutionInput::new(
                initial.turn,
                ResolvedProviderTarget::naming(provider_model_identity(8)),
            )),
            Vec::new(),
        )
        .with_continuation_snapshot(ResolvedContextFrontierReconstitutionInput::new(
            continuation.frontier().owning_session(),
            continuation.frontier().snapshot(),
            continuation.ordered_entries().collect(),
        ))
        .with_tool_result_correlations(vec![ToolResultAttemptCorrelation::new(
            shared_attempt,
            first_request,
            producing_call,
        )]);

        let error = input
            .reconstitute()
            .expect_err("one physical attempt cannot resolve two logical requests");
        assert_eq!(
            error.failure(),
            ModelCallExecutionReconstitutionFailure::ToolResultCorrelationMismatch
        );
    }

    /// S11 / INV-005 / INV-014: a prepared continuation belongs to the most
    /// recent tool round and cannot reuse results from an earlier round.
    #[test]
    fn s11_inv005_inv014_continuation_rejects_unresolved_latest_tool_round() {
        let initial = active_execution();
        let earlier_request = tool_request_id(30);
        let latest_request = tool_request_id(40);
        let earlier_use = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(31),
            initial.session,
            SemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call: model_call_id(32),
                request: earlier_request,
            },
        );
        let earlier_denial = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(33),
            initial.session,
            SemanticTranscriptEntryPayload::ToolDenied {
                request: earlier_request,
            },
        );
        let latest_use = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(34),
            initial.session,
            SemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call: model_call_id(35),
                request: latest_request,
            },
        );
        let continuation = initial
            .starting_snapshot
            .derive_appending_candidate(
                context_frontier_id(36),
                vec![
                    earlier_use.reference(),
                    earlier_denial.reference(),
                    latest_use.reference(),
                ],
            )
            .expect("the malformed continuation still preserves its prefix");
        let attempt = turn_attempt_id(37);
        let turn = initial.turn;
        let selection = *initial.configuration.effective().model();
        let target = ResolvedProviderTarget::naming(provider_model_identity(8));
        let input = ModelCallExecutionReconstitutionInput::new(
            initial
                .active_turn
                .with_phase_for_test(ActiveTurnPhase::Running {
                    current_attempt: CurrentTurnAttempt::prepared(attempt)
                        .begin_running()
                        .expect("the tool tenure is running"),
                }),
            initial.targets.clone(),
            initial.starting_snapshot.clone(),
            vec![
                initial.frontier_entries[0].clone(),
                earlier_use,
                earlier_denial,
                latest_use,
            ],
            initial
                .origin_contents
                .iter()
                .map(|(accepted_input, content)| {
                    ModelCallOriginContent::from_validated_parts(*accepted_input, content.clone())
                })
                .collect(),
            Some(PinnedProviderTargetReconstitutionInput::new(turn, target)),
            vec![ModelCallReconstitutionInput::new(
                model_call_id(38),
                turn,
                attempt,
                selection,
                target,
                continuation.frontier().snapshot(),
                ModelCallReconstitutionState::Prepared,
            )],
        )
        .with_call_snapshot(ResolvedContextFrontierReconstitutionInput::new(
            continuation.frontier().owning_session(),
            continuation.frontier().snapshot(),
            continuation.ordered_entries().collect(),
        ))
        .with_tool_denial_correlations(vec![denied_approval(earlier_request)]);

        let error = input
            .reconstitute()
            .expect_err("an old result cannot close the latest tool round");
        assert_eq!(
            error.failure(),
            ModelCallExecutionReconstitutionFailure::LifecycleMismatch
        );
    }

    /// S07 / S11 / INV-006 / INV-014: a cancellation-only close marker is
    /// terminal history and cannot satisfy ordinary continuation resolution.
    #[test]
    fn s07_s11_inv006_inv014_continuation_rejects_tool_closed() {
        let initial = active_execution();
        let request = tool_request_id(30);
        let attempt = turn_attempt_id(35);
        let call = model_call_id(36);
        let selection = *initial.configuration.effective().model();
        let target = ResolvedProviderTarget::naming(provider_model_identity(8));
        let assistant_tool_use = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(31),
            initial.session,
            SemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call: model_call_id(32),
                request,
            },
        );
        let closed = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(33),
            initial.session,
            SemanticTranscriptEntryPayload::ToolClosed { request },
        );
        let continuation = initial
            .starting_snapshot
            .derive_appending_candidate(
                context_frontier_id(34),
                vec![assistant_tool_use.reference(), closed.reference()],
            )
            .expect("the cancellation frontier preserves its prefix");
        let input = ModelCallExecutionReconstitutionInput::new(
            initial
                .active_turn
                .with_phase_for_test(ActiveTurnPhase::Running {
                    current_attempt: CurrentTurnAttempt::prepared(attempt)
                        .begin_running()
                        .expect("the stored continuation tenure is running"),
                }),
            initial.targets.clone(),
            initial.starting_snapshot.clone(),
            vec![
                initial.frontier_entries[0].clone(),
                assistant_tool_use,
                closed,
            ],
            initial
                .origin_contents
                .into_iter()
                .map(|(accepted_input, content)| {
                    ModelCallOriginContent::from_validated_parts(accepted_input, content)
                })
                .collect(),
            Some(PinnedProviderTargetReconstitutionInput::new(
                initial.turn,
                target,
            )),
            vec![ModelCallReconstitutionInput::new(
                call,
                initial.turn,
                attempt,
                selection,
                target,
                continuation.frontier().snapshot(),
                ModelCallReconstitutionState::Prepared,
            )],
        )
        .with_call_snapshot(ResolvedContextFrontierReconstitutionInput::new(
            continuation.frontier().owning_session(),
            continuation.frontier().snapshot(),
            continuation.ordered_entries().collect(),
        ));

        let error = input
            .reconstitute()
            .expect_err("a tool-close marker cannot reopen cancelled work");
        assert_eq!(
            error.failure(),
            ModelCallExecutionReconstitutionFailure::LifecycleMismatch
        );
    }

    /// S11 / INV-014: a call-free continuation pin is checked against the
    /// immutable target catalog before it can authorize the next provider call.
    #[test]
    fn s11_inv014_continuation_rejects_crosswired_turn_pin() {
        let initial = active_execution();
        let request = tool_request_id(30);
        let assistant_tool_use = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(31),
            initial.session,
            SemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call: model_call_id(32),
                request,
            },
        );
        let denied = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(33),
            initial.session,
            SemanticTranscriptEntryPayload::ToolDenied { request },
        );
        let continuation = initial
            .starting_snapshot
            .derive_appending_candidate(
                context_frontier_id(34),
                vec![assistant_tool_use.reference(), denied.reference()],
            )
            .expect("tool entries preserve the starting prefix");
        let input = ModelCallExecutionReconstitutionInput::new(
            initial
                .active_turn
                .with_phase_for_test(ActiveTurnPhase::Running {
                    current_attempt: CurrentTurnAttempt::prepared(turn_attempt_id(35)),
                }),
            initial.targets,
            initial.starting_snapshot,
            vec![
                initial.frontier_entries[0].clone(),
                assistant_tool_use,
                denied,
            ],
            initial
                .origin_contents
                .into_iter()
                .map(|(accepted_input, content)| {
                    ModelCallOriginContent::from_validated_parts(accepted_input, content)
                })
                .collect(),
            Some(PinnedProviderTargetReconstitutionInput::new(
                initial.turn,
                ResolvedProviderTarget::naming(provider_model_identity(99)),
            )),
            Vec::new(),
        )
        .with_continuation_snapshot(ResolvedContextFrontierReconstitutionInput::new(
            continuation.frontier().owning_session(),
            continuation.frontier().snapshot(),
            continuation.ordered_entries().collect(),
        ))
        .with_tool_denial_correlations(vec![denied_approval(request)]);

        let error = input
            .reconstitute()
            .expect_err("the frozen selection rejects a cross-wired continuation pin");
        assert_eq!(
            error.failure(),
            ModelCallExecutionReconstitutionFailure::CallTargetMismatch
        );
    }

    /// S08 / INV-005 / INV-016: steering correlation considers only the
    /// current turn's suffix and ignores steering retained in its start.
    #[test]
    fn s08_inv005_inv016_reconstitution_ignores_historical_steering() {
        let execution = active_execution();
        let historical_input = accepted_input_id(20);
        let historical = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(21),
            execution.session,
            SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input: historical_input,
                source_turn: turn_id(22),
            },
        );
        let snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
            execution.session,
            context_frontier_id(23),
            vec![
                execution.frontier_entries[0].reference(),
                historical.reference(),
            ],
        )
        .expect("historical steering is valid starting history");
        let start = AcceptedInputTurnStart::from_validated_eligibility(
            crate::AcceptedInputStartingLineage::FirstInSession,
            snapshot.frontier(),
        );
        let mut origin_contents = execution
            .origin_contents
            .into_iter()
            .map(|(accepted_input, content)| {
                ModelCallOriginContent::from_validated_parts(accepted_input, content)
            })
            .collect::<Vec<_>>();
        origin_contents.push(ModelCallOriginContent::from_validated_parts(
            historical_input,
            UserContent::try_text(String::from("historical steering")).expect("valid text"),
        ));
        let input = ModelCallExecutionReconstitutionInput::new(
            execution.active_turn.with_start_for_test(start),
            execution.targets,
            snapshot,
            vec![execution.frontier_entries[0].clone(), historical],
            origin_contents,
            None,
            Vec::new(),
        );

        input
            .reconstitute()
            .expect("historical steering is not current-turn consumed steering");
    }

    /// S02 / INV-005 / INV-015 / INV-036: a call that names a distinct
    /// snapshot must consume a nonempty steering suffix.
    #[test]
    fn s02_inv005_inv015_inv036_reconstitution_rejects_empty_distinct_call_snapshot() {
        let execution = prepared_execution();
        let call = execution
            .current_call()
            .expect("prepared execution has one call");
        let distinct_snapshot = context_frontier_id(25);
        let input = ModelCallExecutionReconstitutionInput::new(
            execution.active_turn.clone(),
            execution.targets.clone(),
            execution.starting_snapshot.clone(),
            execution.frontier_entries.to_vec(),
            execution
                .origin_contents
                .iter()
                .map(|(accepted_input, content)| {
                    ModelCallOriginContent::from_validated_parts(*accepted_input, content.clone())
                })
                .collect(),
            Some(PinnedProviderTargetReconstitutionInput::new(
                call.turn(),
                call.target(),
            )),
            vec![ModelCallReconstitutionInput::new(
                call.id(),
                call.turn(),
                call.attempt(),
                call.selection(),
                call.target(),
                distinct_snapshot,
                ModelCallReconstitutionState::Prepared,
            )],
        )
        .with_call_snapshot(ResolvedContextFrontierReconstitutionInput::new(
            execution.session(),
            distinct_snapshot,
            execution.starting_snapshot.ordered_entries().collect(),
        ));

        let error = input
            .reconstitute()
            .expect_err("a distinct same-content call snapshot consumes no steering");
        assert_eq!(
            error.failure(),
            ModelCallExecutionReconstitutionFailure::CallSnapshotMismatch
        );
    }

    /// S02 / INV-014: persisted target facts must still match immutable
    /// configured target resolution when an execution is reloaded.
    #[test]
    fn s02_inv014_reconstitution_rejects_call_target_crosswired_from_turn_pin() {
        let execution = prepared_execution();
        let call = execution
            .current_call()
            .expect("prepared execution has one call");
        let input = reconstitution_input_with_calls(
            &execution,
            vec![ModelCallReconstitutionInput::new(
                call.id(),
                call.turn(),
                call.attempt(),
                call.selection(),
                ResolvedProviderTarget::naming(provider_model_identity(99)),
                call.frontier().snapshot(),
                ModelCallReconstitutionState::Prepared,
            )],
        );

        let error = input
            .reconstitute()
            .expect_err("stored target drift cannot reconstruct live authority");
        assert_eq!(
            error.failure(),
            ModelCallExecutionReconstitutionFailure::CallTargetMismatch
        );
    }

    /// S02 / INV-014: a call row cannot manufacture the durable target that
    /// belongs independently to its owning turn.
    #[test]
    fn s02_inv014_reconstitution_requires_independent_turn_pin() {
        let execution = prepared_execution();
        let call = execution
            .current_call()
            .expect("prepared execution has one call");
        let mut input = reconstitution_input_with_calls(
            &execution,
            vec![ModelCallReconstitutionInput::new(
                call.id(),
                call.turn(),
                call.attempt(),
                call.selection(),
                call.target(),
                call.frontier().snapshot(),
                ModelCallReconstitutionState::Prepared,
            )],
        );
        input.pinned_target = None;

        let error = input
            .reconstitute()
            .expect_err("a call without its independent turn pin fails closed");
        assert_eq!(
            error.failure(),
            ModelCallExecutionReconstitutionFailure::PinnedTargetMissing
        );
    }

    /// S02 / INV-014: once a call has durably pinned its exact target, a later
    /// deployment-availability change cannot retarget or strand that call.
    #[test]
    fn s02_inv014_prepared_call_reloads_after_target_becomes_unavailable() {
        let execution = prepared_execution();
        let expected_call = execution
            .current_call()
            .expect("prepared execution has one call")
            .clone();
        let mut input = reconstitution_input_with_calls(
            &execution,
            vec![ModelCallReconstitutionInput::new(
                expected_call.id(),
                expected_call.turn(),
                expected_call.attempt(),
                expected_call.selection(),
                expected_call.target(),
                expected_call.frontier().snapshot(),
                ModelCallReconstitutionState::Prepared,
            )],
        );
        input.targets = ModelTargetCatalog::try_from_definitions([])
            .expect("an empty current-availability catalog is valid");

        let reloaded = input
            .reconstitute()
            .expect("durable pinned authority survives current unavailability");

        assert_eq!(reloaded.current_call(), Some(&expected_call));
    }

    /// S02 / INV-014 / INV-015: target resolution records the frozen
    /// selection, target, and exact frontier before send authorization.
    #[test]
    fn s02_inv014_inv015_preparation_is_a_distinct_checkpoint() {
        let execution = active_execution();
        let prepared = execution
            .prepare_initial_call(model_call_id(9))
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

    /// S08 / INV-036: preparation must supply one fresh semantic identity for
    /// every pending steering input in the complete active acceptance tail.
    #[test]
    fn s08_inv036_preparation_requires_the_complete_steering_identity_inventory() {
        let mut execution = active_execution();
        execution.active_turn = execution.active_turn.with_pending_steering_for_test(
            vec![(
                accepted_input_id(20),
                crate::SessionInputPosition::try_from_u64(2)
                    .expect("the test steering position is positive"),
            )]
            .into_boxed_slice(),
        );

        let error = execution
            .prepare_initial_call(model_call_id(9))
            .expect_err("the empty identity inventory cannot consume steering");

        assert_eq!(
            error.failure(),
            ModelCallPreparationFailure::SteeringIdentityCountMismatch
        );
    }

    /// S08 / INV-005 / INV-036: every pending input is consumed in immutable
    /// acceptance order into one prefix extension named by the prepared call.
    #[test]
    fn s08_inv005_inv036_preparation_consumes_multiple_steering_inputs_in_order() {
        let mut execution = active_execution();
        let first = accepted_input_id(20);
        let second = accepted_input_id(21);
        execution.active_turn = execution.active_turn.with_pending_steering_for_test(
            vec![
                (
                    first,
                    crate::SessionInputPosition::try_from_u64(2)
                        .expect("the first steering position is positive"),
                ),
                (
                    second,
                    crate::SessionInputPosition::try_from_u64(3)
                        .expect("the second steering position is positive"),
                ),
            ]
            .into_boxed_slice(),
        );
        execution.origin_contents.insert(
            first,
            UserContent::try_text(String::from("first steering"))
                .expect("the first steering content is valid"),
        );
        execution.origin_contents.insert(
            second,
            UserContent::try_text(String::from("second steering"))
                .expect("the second steering content is valid"),
        );
        let call = model_call_id(9);
        let entry_ids = [
            semantic_transcript_entry_id(22),
            semantic_transcript_entry_id(23),
        ];
        let frontier = context_frontier_id(24);

        let prepared = execution
            .prepare_initial_call_consuming_steering(call, entry_ids.to_vec(), Some(frontier))
            .expect("the complete ordered steering inventory prepares atomically");

        assert_eq!(prepared.call().frontier().snapshot(), frontier);
        assert_eq!(
            prepared
                .consumed_steering()
                .iter()
                .map(|consumed| consumed.accepted_input().id())
                .collect::<Vec<_>>(),
            vec![first, second]
        );
        let first_consumed = &prepared.consumed_steering()[0];
        assert_eq!(
            first_consumed.accepted_input().disposition(),
            &AcceptedInputDisposition::ConsumedAsSteering { call }
        );
        assert_eq!(first_consumed.semantic_entry().identity(), entry_ids[0]);
        assert_eq!(
            first_consumed.semantic_entry().payload(),
            &SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input: first,
                source_turn: turn_id(3),
            }
        );
        let second_consumed = &prepared.consumed_steering()[1];
        assert_eq!(
            second_consumed.accepted_input().disposition(),
            &AcceptedInputDisposition::ConsumedAsSteering { call }
        );
        assert_eq!(second_consumed.semantic_entry().identity(), entry_ids[1]);
        assert_eq!(
            second_consumed.semantic_entry().payload(),
            &SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input: second,
                source_turn: turn_id(3),
            }
        );
        assert_eq!(
            prepared
                .steering_snapshot()
                .expect("steering creates an extended frontier")
                .ordered_entries()
                .collect::<Vec<_>>(),
            vec![
                SemanticTranscriptEntryRef::from_source(
                    session_id(1),
                    semantic_transcript_entry_id(5),
                ),
                SemanticTranscriptEntryRef::from_source(session_id(1), entry_ids[0]),
                SemanticTranscriptEntryRef::from_source(session_id(1), entry_ids[1]),
            ]
        );
    }

    /// S02 / INV-006 / INV-014: an immutable-catalog miss is retained as the
    /// exact proof authorizing known-failure closure before any call exists.
    #[test]
    fn s02_inv006_inv014_target_resolution_failure_requires_matching_proof() {
        let mut execution = active_execution();
        execution.targets =
            ModelTargetCatalog::try_from_definitions([]).expect("the empty test catalog is valid");
        let preparation = execution
            .prepare_initial_call(model_call_id(9))
            .expect_err("the configured selection is unavailable");
        let proof = preparation
            .target_resolution_error()
            .expect("target unavailability retains the exact catalog miss");

        let failed = preparation
            .execution()
            .clone()
            .fail_target_resolution(
                proof,
                FailedModelCallTurnIdentities::new(
                    semantic_transcript_entry_id(10),
                    context_frontier_id(11),
                ),
            )
            .expect("the matching catalog miss authorizes known-failure closure");

        assert!(failed.call().is_none());
        assert_eq!(failed.disposition(), &TurnDisposition::Failed);
    }

    /// S02 / INV-006 / INV-014: a catalog miss obtained elsewhere cannot
    /// discard a turn whose own immutable catalog resolves successfully.
    #[test]
    fn s02_inv006_inv014_resolvable_turn_rejects_foreign_resolution_failure() {
        let execution = active_execution();
        let foreign_proof = ModelTargetCatalog::try_from_definitions([])
            .expect("the empty test catalog is valid")
            .resolve(*execution.configuration().effective().model())
            .expect_err("the foreign empty catalog cannot resolve the selection");

        let error = execution
            .fail_target_resolution(
                foreign_proof,
                FailedModelCallTurnIdentities::new(
                    semantic_transcript_entry_id(10),
                    context_frontier_id(11),
                ),
            )
            .expect_err("another catalog's miss cannot terminalize a resolvable turn");

        assert_eq!(error, ModelCallClosureError::TargetResolutionMismatch);
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

    /// S02 / INV-005: resuming a prepared call renders only content named by
    /// that call's immutable frontier, excluding steering accepted later.
    #[test]
    fn s02_inv005_prepared_request_excludes_later_pending_steering_content() {
        let mut execution = prepared_execution_consuming_steering();
        let later = accepted_input_id(21);
        execution.active_turn = execution.active_turn.with_pending_steering_for_test(
            vec![(
                later,
                crate::SessionInputPosition::try_from_u64(3)
                    .expect("the later steering position is positive"),
            )]
            .into_boxed_slice(),
        );
        execution.origin_contents.insert(
            later,
            UserContent::try_text(String::from("later steering"))
                .expect("the later steering content is valid"),
        );

        let request = execution
            .resume_prepared_call()
            .expect("the committed prepared call remains resumable");

        assert!(request.origin_content(later).is_none());
        assert_eq!(request.origin_contents.len(), 2);
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
        let correlation = authorized.observation_correlation();
        assert_eq!(correlation.session(), authorized.session());
        assert_eq!(correlation.turn(), authorized.turn());
        assert_eq!(correlation.attempt(), authorized.attempt().id());
        assert_eq!(correlation.call(), authorized.call().id());
        assert_eq!(correlation.target(), authorized.call().target());
        assert_eq!(
            correlation.frontier(),
            authorized.call().frontier().snapshot()
        );
        let observation =
            correlation.bind_terminal_observation(ModelCallTerminalObservation::KnownFailed);
        assert_eq!(observation.call(), authorized.call().id());
        assert_eq!(observation.correlation(), &correlation);
        assert_eq!(
            observation.observation(),
            &ModelCallTerminalObservation::KnownFailed
        );
    }

    /// S07 / INV-006 / INV-029 / INV-037: interruption before a physical
    /// call exists ends the attempt and turn directly with the sole applied
    /// proof and one explicit cancellation marker.
    #[test]
    fn s07_inv006_inv029_inv037_interrupt_cancels_unprepared_work_directly() {
        let execution = active_execution();
        let interrupt = applied_interrupt(&execution);
        let expected_turn = execution.turn();
        let outcome = execution
            .apply_interrupt(
                interrupt,
                CancelledModelCallTurnIdentities::new(
                    semantic_transcript_entry_id(33),
                    context_frontier_id(34),
                ),
            )
            .expect("a matching interrupt cancels unsent work");
        let ModelCallInterruptOutcome::Cancelled(cancelled) = outcome else {
            panic!("unsent work is terminally cancelled");
        };

        assert!(cancelled.call().is_none());
        assert_eq!(
            cancelled.attempt().end(),
            &crate::AttemptEnd::AfterCancellation {
                cause: interrupt.proof(),
                disposition: CancellationStopDisposition::Cancelled,
            }
        );
        assert_eq!(
            cancelled.disposition(),
            &TurnDisposition::Cancelled {
                cause: interrupt.proof(),
            }
        );
        assert!(matches!(
            cancelled.cancellation_entry().payload(),
            SemanticTranscriptEntryPayload::TurnCancelled { turn }
                if *turn == expected_turn
        ));
    }

    /// S07 / INV-005 / INV-029: interrupt result projection is bound to the
    /// exact yielded frontier identity, not merely equal semantic content.
    #[test]
    fn s07_inv005_inv029_tool_cancellation_rejects_same_content_foreign_frontier() {
        let execution = active_execution();
        let foreign_yield = ResolvedContextFrontierSnapshot::try_from_candidate(
            execution.session(),
            context_frontier_id(40),
            execution.current_snapshot.ordered_entries().collect(),
        )
        .expect("same-content foreign frontier is structurally valid");
        let request = batch_request(41, &execution);
        let projection = ToolBatchReconstitutionInput::new(
            execution.session(),
            execution.turn(),
            request.producing_call(),
            foreign_yield,
            vec![request.clone()],
            vec![denied_approval(request.id())],
            vec![],
            ToolBatchPhaseReconstitutionInput::Executing {
                turn_attempt: execution.current_attempt().id(),
            },
        )
        .reconstitute()
        .expect("the denied fixture batch is complete")
        .prepare_cancellation_projection(
            vec![semantic_transcript_entry_id(42)],
            context_frontier_id(43),
        )
        .expect("the foreign batch can prepare its own cancellation projection");
        let interrupt = applied_interrupt(&execution);

        let error = execution
            .apply_interrupt_to_tool_batch(
                interrupt,
                projection,
                CancelledModelCallTurnIdentities::new(
                    semantic_transcript_entry_id(44),
                    context_frontier_id(45),
                ),
            )
            .expect_err("same semantic content cannot substitute another yielded frontier");
        assert_eq!(error, ModelCallClosureError::InterruptCorrelationMismatch);
    }

    /// S07 / INV-006 / INV-029 / INV-037: a prepared but unsent call closes
    /// as proof-bearing cancellation without crossing send authorization.
    #[test]
    fn s07_inv006_inv029_inv037_interrupt_cancels_prepared_call_directly() {
        let execution = prepared_execution();
        let interrupt = applied_interrupt(&execution);
        let outcome = execution
            .apply_interrupt(
                interrupt,
                CancelledModelCallTurnIdentities::new(
                    semantic_transcript_entry_id(33),
                    context_frontier_id(34),
                ),
            )
            .expect("a matching interrupt cancels a prepared call");
        let ModelCallInterruptOutcome::Cancelled(cancelled) = outcome else {
            panic!("a prepared call is terminally cancelled");
        };

        assert_eq!(
            cancelled
                .call()
                .expect("the prepared call remains immutable history")
                .disposition(),
            ModelCallDisposition::Cancelled
        );
        assert_eq!(
            cancelled.attempt().end(),
            &crate::AttemptEnd::AfterCancellation {
                cause: interrupt.proof(),
                disposition: CancellationStopDisposition::Cancelled,
            }
        );
    }

    /// S07 / INV-006 / INV-029 / INV-037: issued work durably records the
    /// same cancellation authority on the attempt and call while retaining
    /// the active slot.
    #[test]
    fn s07_inv006_inv029_inv037_interrupt_requests_issued_call_cancellation() {
        let execution = in_flight_execution();
        let interrupt = applied_interrupt(&execution);
        let outcome = execution
            .apply_interrupt(
                interrupt,
                CancelledModelCallTurnIdentities::new(
                    semantic_transcript_entry_id(33),
                    context_frontier_id(34),
                ),
            )
            .expect("a matching interrupt stops issued work");
        let ModelCallInterruptOutcome::CancellationRequested(stopped) = outcome else {
            panic!("issued work retains the slot while cancellation is requested");
        };

        assert_eq!(
            stopped.attempt().state(),
            &CurrentTurnAttemptState::StopRequested {
                causes: TurnAttemptStopCauses::CancellationOnly {
                    interrupt: interrupt.proof(),
                },
            }
        );
        assert_eq!(
            stopped.call().state(),
            CurrentModelCallState::CancellationRequested
        );
        assert_eq!(stopped.interrupt(), interrupt.proof());
    }

    /// S07 / INV-006 / INV-029 / INV-037: confirmed physical cancellation
    /// after a durable stop request is the evidence that releases the slot as
    /// `Cancelled`.
    #[test]
    fn s07_inv006_inv029_inv037_confirmed_cancellation_terminalizes_stopped_call() {
        let (execution, interrupt) = stop_requested_execution(in_flight_execution());
        let observation =
            correlated_observation(&execution, ModelCallTerminalObservation::Cancelled);
        let outcome = execution
            .apply_terminal_observation(
                observation,
                ModelCallTerminalIdentities::PhysicalCancellation(
                    PhysicalCancellationModelCallTurnIdentities::new(
                        semantic_transcript_entry_id(35),
                        context_frontier_id(36),
                    ),
                ),
            )
            .expect("confirmed cancellation closes the stopped call");
        let ModelCallTerminalOutcome::Cancelled(cancelled) = outcome else {
            panic!("physical cancellation plus exact proof cancels the turn");
        };

        assert_eq!(
            cancelled
                .call()
                .expect("the issued call remains terminal history")
                .disposition(),
            ModelCallDisposition::Cancelled
        );
        assert_eq!(
            cancelled.attempt().end(),
            &crate::AttemptEnd::AfterCancellation {
                cause: interrupt.proof(),
                disposition: CancellationStopDisposition::Cancelled,
            }
        );
    }

    /// S07 / INV-006 / INV-029: outcome-authoritative completion racing a
    /// stop request wins while retaining the interrupt in attempt history.
    #[test]
    fn s07_inv006_inv029_completion_race_preserves_outcome_and_stop_history() {
        let (execution, interrupt) = stop_requested_execution(in_flight_execution());
        let observation = correlated_observation(
            &execution,
            ModelCallTerminalObservation::Completed {
                assistant_text: vec![
                    AssistantText::try_new("race winner".to_owned()).expect("nonempty text"),
                ],
            },
        );
        let outcome = execution
            .apply_terminal_observation(
                observation,
                ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                    vec![semantic_transcript_entry_id(35)],
                    semantic_transcript_entry_id(36),
                    context_frontier_id(37),
                )),
            )
            .expect("definitive completion wins the cancellation race");
        let ModelCallTerminalOutcome::Completed(completed) = outcome else {
            panic!("definitive completion remains authoritative");
        };

        assert_eq!(
            completed.attempt().end(),
            &crate::AttemptEnd::AfterCancellation {
                cause: interrupt.proof(),
                disposition: CancellationStopDisposition::TurnCompleted,
            }
        );
    }

    /// S07 / S11 / INV-006 / INV-027 / INV-029: a tool-using response racing
    /// an applied interrupt records its proposals, closes them without
    /// attempts, and terminalizes through the original stop proof.
    #[test]
    fn s07_s11_inv006_inv027_inv029_tool_response_race_closes_without_execution() {
        let (execution, interrupt) = stop_requested_execution(in_flight_execution());
        let request = tool_request_id(40);
        let expected_turn = execution.turn();
        let observation = correlated_observation(
            &execution,
            ModelCallTerminalObservation::CompletedWithTools {
                response: ToolUsingAssistantResponse::try_from_parts(vec![
                    AssistantResponsePart::ToolCall(tool_proposal("risky_tool", "{}")),
                ])
                .expect("the response contains one tool proposal"),
            },
        );
        let outcome = execution
            .apply_terminal_observation(
                observation,
                ModelCallTerminalIdentities::StoppedToolRound(
                    StoppedToolRoundModelCallIdentities::new(
                        vec![StoppedToolResponsePartIdentity::tool_call(
                            semantic_transcript_entry_id(41),
                            request,
                            semantic_transcript_entry_id(42),
                        )],
                        semantic_transcript_entry_id(43),
                        context_frontier_id(44),
                    ),
                ),
            )
            .expect("the stop proof closes newly proposed tools");
        let ModelCallTerminalOutcome::CancelledWithToolResponse(cancelled) = outcome else {
            panic!("a stopped tool response terminalizes through cancellation");
        };

        assert_eq!(
            cancelled.attempt().end(),
            &AttemptEnd::AfterCancellation {
                cause: interrupt.proof(),
                disposition: CancellationStopDisposition::Cancelled,
            }
        );
        assert_eq!(cancelled.requests()[0].id(), request);
        assert_eq!(
            cancelled.closed_result_entries()[0].payload(),
            &SemanticTranscriptEntryPayload::ToolClosed { request }
        );
        assert!(matches!(
            cancelled.cancellation_entry().payload(),
            SemanticTranscriptEntryPayload::TurnCancelled { turn }
                if *turn == expected_turn
        ));
    }

    /// S04 / S07 / INV-025 / INV-029: an applied interrupt makes
    /// unacknowledged call ambiguity terminal reconciliation, preserving the
    /// exact operation and stop proof while releasing the slot.
    #[test]
    fn s04_s07_inv025_inv029_stopped_ambiguity_requires_reconciliation() {
        let pending = accepted_input_id(40);
        let execution = with_pending_steering(in_flight_execution(), pending);
        let source_turn = execution.turn();
        let (execution, interrupt) = stop_requested_execution(execution);
        let observation =
            correlated_observation(&execution, ModelCallTerminalObservation::Ambiguous);
        let outcome = execution
            .apply_terminal_observation(
                observation,
                ModelCallTerminalIdentities::Ambiguous(
                    AmbiguousModelCallTurnIdentities::new(context_frontier_id(41))
                        .with_pending_steering_reclassifications(one_reclassification(
                            pending,
                            turn_id(42),
                        )),
                ),
            )
            .expect("stopped ambiguity is exactly representable");
        let ModelCallTerminalOutcome::ReconciliationRequired(reconciliation) = outcome else {
            panic!("stopped ambiguity must release the slot through reconciliation");
        };

        assert_eq!(
            reconciliation.attempt().end(),
            &crate::AttemptEnd::AfterCancellation {
                cause: interrupt.proof(),
                disposition: CancellationStopDisposition::Ambiguous,
            }
        );
        let TurnDisposition::ReconciliationRequired { marker } = reconciliation.disposition()
        else {
            panic!("the terminal disposition carries its complete marker");
        };
        assert_eq!(
            marker.reason(),
            &crate::ReconciliationReason::InterruptRequiresReconciliation {
                interrupt: interrupt.proof(),
            }
        );
        assert_eq!(marker.ambiguous_operations().operation_count(), 1);
        assert!(
            marker
                .ambiguous_operations()
                .contains(crate::IssuedOperationRef::ModelCall(
                    reconciliation.call().id()
                ))
        );
        assert_one_reclassified_turn(
            reconciliation.reclassified_pending_steering(),
            pending,
            source_turn,
            turn_id(42),
        );
    }

    /// S02 / INV-006 / INV-014 / INV-034: an authoritative reread of a durably
    /// issued call reconstructs the same provider-facing correlation without
    /// authorizing or transitioning it a second time.
    #[test]
    fn s02_inv006_inv014_inv034_in_flight_reread_reconstructs_exact_authorization() {
        let execution = in_flight_execution();
        let expected_call = execution
            .current_call()
            .expect("the fixture contains one issued call")
            .id();
        let authorized = execution
            .resume_in_flight_call()
            .expect("checked InFlight state is resumable for reread only");

        assert_eq!(authorized.call().id(), expected_call);
        assert_eq!(authorized.call().state(), CurrentModelCallState::InFlight);
        assert_eq!(
            authorized.attempt().state(),
            &CurrentTurnAttemptState::Running
        );
        assert_eq!(authorized.observation_correlation().call(), expected_call);
        assert_eq!(authorized.session(), execution.session());
        assert_eq!(authorized.turn(), execution.turn());
        assert_eq!(authorized.attempt(), execution.current_attempt());
        assert_eq!(authorized.call(), execution.current_call().unwrap());
        assert!(prepared_execution().resume_in_flight_call().is_none());
    }

    /// S02 / INV-006 / INV-014: a provider observation remains bound to the
    /// exact session, turn, attempt, call, target, and frontier that crossed
    /// send authorization.
    #[test]
    fn s02_inv006_inv014_terminal_observation_rejects_cross_wired_call() {
        let execution = in_flight_execution();
        let mut observation =
            correlated_observation(&execution, ModelCallTerminalObservation::KnownFailed);
        observation.correlation.call = model_call_id(99);

        let error = execution
            .apply_terminal_observation(
                observation,
                ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                    semantic_transcript_entry_id(10),
                    context_frontier_id(11),
                )),
            )
            .expect_err("another call's observation cannot close fresh authority");

        assert_eq!(error, ModelCallClosureError::ObservationCorrelationMismatch);
    }

    /// S02 / INV-005 / INV-006 / INV-032: successful final text, physical
    /// completion, attempt/turn completion, and the final marker share one
    /// prefix-preserving candidate.
    #[test]
    fn s02_inv005_inv006_inv032_completion_is_atomic_and_ordered() {
        let execution = in_flight_execution();
        let observation = correlated_observation(
            &execution,
            ModelCallTerminalObservation::Completed {
                assistant_text: vec![
                    AssistantText::try_new("first".to_string()).expect("nonempty text"),
                    AssistantText::try_new(" second ".to_string()).expect("nonempty text"),
                ],
            },
        );
        let outcome = execution
            .apply_terminal_observation(
                observation,
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

    /// S02 / S10 / INV-005 / INV-006 / INV-019: a tool-using completion
    /// commits ordered request references, yields its attempt, and parks on
    /// the earliest undecided request without completing the turn.
    #[test]
    fn s02_s10_inv005_inv006_inv019_tool_round_yields_and_parks_in_order() {
        let execution = in_flight_execution();
        let first_request = tool_request_id(20);
        let second_request = tool_request_id(21);
        let observation = correlated_observation(
            &execution,
            ModelCallTerminalObservation::CompletedWithTools {
                response: ToolUsingAssistantResponse::try_from_parts(vec![
                    AssistantResponsePart::Text(
                        AssistantText::try_new(String::from("checking"))
                            .expect("assistant text is nonempty"),
                    ),
                    AssistantResponsePart::ToolCall(tool_proposal(
                        "risky_tool",
                        r#"{"b":2,"a":1}"#,
                    )),
                    AssistantResponsePart::ToolCall(tool_proposal("current_time", "{}")),
                ])
                .expect("the response contains tool proposals"),
            },
        );
        let outcome = execution
            .apply_terminal_observation(
                observation,
                ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                    vec![
                        ToolResponsePartIdentity::text(semantic_transcript_entry_id(10)),
                        ToolResponsePartIdentity::tool_call(
                            semantic_transcript_entry_id(11),
                            first_request,
                            InitialToolApproval::Confirm,
                        ),
                        ToolResponsePartIdentity::tool_call(
                            semantic_transcript_entry_id(12),
                            second_request,
                            InitialToolApproval::PolicyAuto,
                        ),
                    ],
                    context_frontier_id(13),
                    None,
                )),
            )
            .expect("ordered request content and identities produce one tool yield");
        let ModelCallTerminalOutcome::ToolRound(round) = outcome else {
            panic!("tool-using completion yields a tool round");
        };

        assert_eq!(
            round.attempt().end(),
            &AttemptEnd::WithoutStop {
                disposition: UnstoppedAttemptDisposition::YieldedToDurableWait,
            }
        );
        assert!(matches!(
            round.next_phase(),
            ActiveTurnPhase::AwaitingApproval { request } if *request == first_request
        ));
        assert_eq!(round.requests()[0].id(), first_request);
        assert_eq!(
            round.requests()[0].ordinal(),
            ToolRequestOrdinal::from_u32(0)
        );
        assert_eq!(round.requests()[0].arguments().as_str(), r#"{"a":1,"b":2}"#);
        assert_eq!(round.requests()[1].id(), second_request);
        assert_eq!(
            round.requests()[1].ordinal(),
            ToolRequestOrdinal::from_u32(1)
        );
        assert_eq!(round.automatic_approvals().len(), 1);
        assert_eq!(
            round.automatic_approvals()[0].source(),
            crate::ToolDecisionSource::PolicyAuto
        );
        assert!(matches!(
            round.assistant_entries()[1].payload(),
            SemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            } if *producing_call == model_call_id(9) && *request == first_request
        ));
        assert_eq!(
            round
                .yielded_snapshot()
                .ordered_entries()
                .collect::<Vec<_>>(),
            vec![
                SemanticTranscriptEntryRef::from_source(
                    session_id(1),
                    semantic_transcript_entry_id(5),
                ),
                SemanticTranscriptEntryRef::from_source(
                    session_id(1),
                    semantic_transcript_entry_id(10),
                ),
                SemanticTranscriptEntryRef::from_source(
                    session_id(1),
                    semantic_transcript_entry_id(11),
                ),
                SemanticTranscriptEntryRef::from_source(
                    session_id(1),
                    semantic_transcript_entry_id(12),
                ),
            ]
        );
    }

    /// S10 / INV-001 / INV-006: a later model round cannot reuse a tool
    /// request identity already present in immutable transcript history.
    #[test]
    fn s10_inv001_inv006_tool_round_rejects_historical_request_identity() {
        let execution = in_flight_execution();
        let request = tool_request_id(20);
        let mut frontier_entries = execution.frontier_entries.to_vec();
        frontier_entries.push(SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(40),
            execution.session(),
            SemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call: model_call_id(41),
                request,
            },
        ));
        frontier_entries.push(SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(42),
            execution.session(),
            SemanticTranscriptEntryPayload::ToolDenied { request },
        ));
        let call = execution
            .current_call
            .clone()
            .expect("the fixture has an issued call")
            .end_classified(ModelCallDisposition::Completed)
            .expect("issued calls accept completed classification");
        let attempt = execution
            .current_attempt
            .clone()
            .end_without_stop(UnstoppedAttemptDisposition::YieldedToDurableWait)
            .expect("the running fixture can yield");
        let response =
            ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
                tool_proposal("current_time", "{}"),
            )])
            .expect("the response contains one tool proposal");

        let error = assemble_tool_round(
            ModelCallTurnScope {
                session: execution.session(),
                turn: execution.turn(),
            },
            call,
            attempt,
            frontier_entries,
            response,
            ToolRoundModelCallIdentities::new(
                vec![ToolResponsePartIdentity::tool_call(
                    semantic_transcript_entry_id(43),
                    request,
                    InitialToolApproval::Confirm,
                )],
                context_frontier_id(44),
                None,
            ),
            DangerousToolAutoApproval::Disabled,
        )
        .expect_err("immutable request identity cannot be reused");

        assert_eq!(error, ModelCallClosureError::FrontierDerivationFailed);
    }

    /// S02 / S15 / INV-006 / INV-009: an all-auto batch creates one fresh
    /// prepared continuation attempt while retaining the same logical turn.
    #[test]
    fn s02_s15_inv006_inv009_all_auto_tool_round_prepares_continuation() {
        let execution = in_flight_execution();
        let request = tool_request_id(20);
        let continuation = turn_attempt_id(21);
        let observation = correlated_observation(
            &execution,
            ModelCallTerminalObservation::CompletedWithTools {
                response: ToolUsingAssistantResponse::try_from_parts(vec![
                    AssistantResponsePart::ToolCall(tool_proposal("current_time", "{}")),
                ])
                .expect("the response contains one tool proposal"),
            },
        );
        let outcome = execution
            .apply_terminal_observation(
                observation,
                ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                    vec![ToolResponsePartIdentity::tool_call(
                        semantic_transcript_entry_id(10),
                        request,
                        InitialToolApproval::PolicyAuto,
                    )],
                    context_frontier_id(11),
                    Some(continuation),
                )),
            )
            .expect("an all-auto batch has no approval wait");
        let ModelCallTerminalOutcome::ToolRound(round) = outcome else {
            panic!("tool-using completion yields a tool round");
        };
        let ActiveTurnPhase::Running { current_attempt } = round.next_phase() else {
            panic!("all-auto policy prepares continuation");
        };

        assert_eq!(round.turn(), turn_id(3));
        assert_eq!(current_attempt.id(), continuation);
        assert_eq!(current_attempt.state(), &CurrentTurnAttemptState::Prepared);
    }

    /// S08 / INV-016: a definitive response terminalizes its source only
    /// together with ordered, visible reclassification of pending steering.
    #[test]
    fn s08_inv016_completion_reclassifies_pending_steering_atomically() {
        let pending = accepted_input_id(20);
        let successor = turn_id(21);
        let execution = with_pending_steering(in_flight_execution(), pending);
        let observation = correlated_observation(
            &execution,
            ModelCallTerminalObservation::Completed {
                assistant_text: vec![
                    AssistantText::try_new("reply".to_owned()).expect("nonempty text"),
                ],
            },
        );
        let identities = CompletedModelCallIdentities::new(
            vec![semantic_transcript_entry_id(10)],
            semantic_transcript_entry_id(11),
            context_frontier_id(12),
        )
        .with_pending_steering_reclassifications(one_reclassification(pending, successor));

        let outcome = execution
            .apply_terminal_observation(
                observation,
                ModelCallTerminalIdentities::Completed(identities),
            )
            .expect("terminal completion may reclassify complete steering facts");
        let ModelCallTerminalOutcome::Completed(completed) = outcome else {
            panic!("completed evidence selects completed outcome");
        };

        assert_one_reclassified_turn(
            completed.reclassified_pending_steering(),
            pending,
            turn_id(3),
            successor,
        );
    }

    /// S08 / INV-016: terminal observation cannot release the source while a
    /// pending input lacks its exact reclassified successor identity.
    #[test]
    fn s08_inv016_terminal_observation_rejects_missing_reclassification() {
        let pending = accepted_input_id(20);
        let execution = with_pending_steering(in_flight_execution(), pending);
        let observation =
            correlated_observation(&execution, ModelCallTerminalObservation::KnownFailed);

        let error = execution
            .apply_terminal_observation(
                observation,
                ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                    semantic_transcript_entry_id(10),
                    context_frontier_id(11),
                )),
            )
            .expect_err("pending steering cannot disappear at terminalization");

        assert_eq!(
            error,
            ModelCallClosureError::PendingSteeringReclassificationMismatch
        );
    }

    /// S08 / INV-016: a refusal reclassifies pending steering without adding
    /// response content to the refused turn's terminal frontier.
    #[test]
    fn s08_inv016_refusal_reclassifies_pending_steering_atomically() {
        let pending = accepted_input_id(20);
        let successor = turn_id(21);
        let execution = with_pending_steering(in_flight_execution(), pending);
        let observation = correlated_observation(&execution, ModelCallTerminalObservation::Refused);
        let identities = RefusedModelCallTurnIdentities::new(context_frontier_id(10))
            .with_pending_steering_reclassifications(one_reclassification(pending, successor));

        let outcome = execution
            .apply_terminal_observation(
                observation,
                ModelCallTerminalIdentities::Refused(identities),
            )
            .expect("terminal refusal may reclassify complete steering facts");
        let ModelCallTerminalOutcome::Refused(refused) = outcome else {
            panic!("refused evidence selects refused outcome");
        };

        assert_one_reclassified_turn(
            refused.reclassified_pending_steering(),
            pending,
            turn_id(3),
            successor,
        );
    }

    /// S08 / INV-016: trustworthy pre-send failure releases its source only
    /// together with pending-steering reclassification.
    #[test]
    fn s08_inv016_prepared_failure_reclassifies_pending_steering_atomically() {
        let pending = accepted_input_id(20);
        let successor = turn_id(21);
        let execution = with_pending_steering(prepared_execution(), pending);
        let identities = FailedModelCallTurnIdentities::new(
            semantic_transcript_entry_id(10),
            context_frontier_id(11),
        )
        .with_pending_steering_reclassifications(one_reclassification(pending, successor));

        let failed = execution
            .fail_prepared_call(identities)
            .expect("pre-send failure may reclassify complete steering facts");

        assert_one_reclassified_turn(
            failed.reclassified_pending_steering(),
            pending,
            turn_id(3),
            successor,
        );
    }

    /// S08 / INV-005 / INV-036: a known failure after steering consumption
    /// appends its marker to the call frontier without losing consumed input.
    #[test]
    fn s08_inv005_inv036_prepared_failure_extends_steering_call_frontier() {
        let failure_entry = semantic_transcript_entry_id(30);
        let failed = prepared_execution_consuming_steering()
            .fail_prepared_call(FailedModelCallTurnIdentities::new(
                failure_entry,
                context_frontier_id(31),
            ))
            .expect("known failure extends the exact prepared call frontier");

        assert_eq!(
            failed
                .terminal_snapshot()
                .ordered_entries()
                .collect::<Vec<_>>(),
            vec![
                SemanticTranscriptEntryRef::from_source(
                    session_id(1),
                    semantic_transcript_entry_id(5),
                ),
                SemanticTranscriptEntryRef::from_source(
                    session_id(1),
                    semantic_transcript_entry_id(22),
                ),
                SemanticTranscriptEntryRef::from_source(session_id(1), failure_entry),
            ]
        );
    }

    /// S04 / INV-025 / INV-026: ambiguous physical completion ends the live
    /// attempt and retains the exact call in a durable recovery wait.
    #[test]
    fn s04_inv025_inv026_ambiguity_preserves_call_and_waits() {
        let execution = in_flight_execution();
        let observation =
            correlated_observation(&execution, ModelCallTerminalObservation::Ambiguous);
        let outcome = execution
            .apply_terminal_observation(
                observation,
                ModelCallTerminalIdentities::Ambiguous(AmbiguousModelCallTurnIdentities::new(
                    context_frontier_id(43),
                )),
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

    /// S04 / S08 / INV-016 / INV-034: startup converts an unsent prepared call
    /// to known failure, records the lost attempt, and reclassifies steering
    /// before releasing the source.
    #[test]
    fn s04_s08_inv016_inv034_restart_closes_prepared_call_and_reclassifies_steering() {
        let pending = accepted_input_id(20);
        let successor = turn_id(21);
        let execution = with_pending_steering(prepared_execution(), pending);
        let identities = FailedModelCallTurnIdentities::new(
            semantic_transcript_entry_id(10),
            context_frontier_id(11),
        )
        .with_pending_steering_reclassifications(one_reclassification(pending, successor));
        let outcome = execution
            .recover_after_restart(identities)
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
        assert_one_reclassified_turn(
            failed.reclassified_pending_steering(),
            pending,
            turn_id(3),
            successor,
        );
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

    /// S02 / S04 / INV-006 / INV-014: cancellation-requested call state lacks
    /// the proof-bearing stopped-attempt facts required by
    /// docs/spec/turn-lifecycle-and-scheduling.md, so this evidence-free
    /// execution projection fails closed during reconstitution.
    #[test]
    fn s02_s04_inv006_inv014_cancellation_requested_reconstitution_fails_closed() {
        let in_flight = in_flight_execution();
        let cancellation_requested = in_flight
            .current_call()
            .expect("in-flight execution has one call")
            .clone()
            .request_cancellation()
            .expect("an in-flight call may request cancellation");
        let error = reconstitution_input_with_calls(
            &in_flight,
            vec![ModelCallReconstitutionInput::new(
                cancellation_requested.id(),
                cancellation_requested.turn(),
                cancellation_requested.attempt(),
                cancellation_requested.selection(),
                cancellation_requested.target(),
                cancellation_requested.frontier().snapshot(),
                ModelCallReconstitutionState::CancellationRequested,
            )],
        )
        .reconstitute()
        .expect_err("proof-free cancellation-requested storage must not reconstruct live");

        assert_eq!(
            error.failure(),
            ModelCallExecutionReconstitutionFailure::LifecycleMismatch
        );
    }

    /// S02 / INV-006: definitive known failure closes the physical call and
    /// logical turn as failed in one candidate.
    #[test]
    fn s02_inv006_known_failure_closes_call_attempt_and_turn() {
        let execution = in_flight_execution();
        let observation =
            correlated_observation(&execution, ModelCallTerminalObservation::KnownFailed);
        let outcome = execution
            .apply_terminal_observation(
                observation,
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
        let execution = in_flight_execution();
        let observation =
            correlated_observation(&execution, ModelCallTerminalObservation::Cancelled);
        let outcome = execution
            .apply_terminal_observation(
                observation,
                ModelCallTerminalIdentities::PhysicalCancellation(
                    PhysicalCancellationModelCallTurnIdentities::new(
                        semantic_transcript_entry_id(10),
                        context_frontier_id(11),
                    ),
                ),
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
        let execution = in_flight_execution();
        let observation = correlated_observation(&execution, ModelCallTerminalObservation::Refused);
        let outcome = execution
            .apply_terminal_observation(
                observation,
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
